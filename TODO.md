# TODO — Caprock open items

> **This file is the merge of `todo.md` (Part I) and `TODO0.md` (Part II)**, both translated into
> English, done on 2026-10-19. Section identifiers (`A3`, `Z14`, `K1c`, `D13` …) are unchanged; a
> reference to "TODO0 K1c" now means *Part II, K1c*.
>
> **`done.md` was removed in the same change.** It held what was finished, together with the assumptions
> that turned out to be wrong, and it was the largest file in the repository. It is **not lost**: it is in
> the git history (present up to and including commit `e9d7054`), and the entries that cited it keep
> their wording ("see `done.md`") on purpose — to read it: `git show e9d7054:done.md`.

# Part I — Open items and the target architecture (formerly `todo.md`)

Only **what is not yet done**. What is finished stood, with reasons, in `done.md`, which was removed on
2026-10-19 — its last version is in the git history (see the note at the top of this file).
Order within a section = priority. `[~]` = partly done, remainder named.

**A strand plan follows in Part II** (formerly `TODO0.md`: Linux userland and local graphics,
2026-08-17). It is a **plan, not a second register** — it copies nothing from Part I, but
points (Z14, Z16, Z19, Z20, Z26/A3, Z27, Z28) and keeps its own items only for what does not
stand in Part I yet. If one of them is begun, it moves **into Part I**. Where both say something,
Part I applies.

**Tidied on 2026-08-11:** 6 completely finished entries and 54 individual `[x]` items moved
**verbatim** to done.md (section "Tidied over from `TODO.md`"), together with
their reasons — those are the value, not the ticks. Afterwards there is **not a single
`[x]`** left here; the file answers the question "what is open" again, without anyone having to
read past finished items. State afterwards: **138 open** and **16 partly done** items.

---

## B4b. The device interrupt does not arrive — and the kernel is checked without gaps

**Class:** measurement rig / stage B · **State:** B1–B3 measured, B4 built and **not effective**
(`B4_WARTEN = false` in `programs/hardware/virtio-blk`).

The driver binds its interrupt via `SYS_BIND_IRQ` and does not yet wait. The reason is a chain of
quantities **read back**, no conjecture:

| Link | measured |
|---|---|
| Queue 0 → MSI-X row | `queue0-msix-vektor=0x0000` |
| Row 0 in the device | `addr=0xfee00008`, `vctrl=0x0` (unmasked) |
| Message control | `0x8004` — enable on, **function mask free** |
| IRTE handle 0 | present, vector `0x60`, `SVT=01`, SID = RID `0x0018` |
| IEC invalidation | done |
| Work of the device | **`used.idx=1`** |
| IOMMU faults | **empty** |
| Delivery path behind it | **proved** (self-IPI, vector `0x64` reaches `irq_hook` and the notification) |

**The device does the request and sends nothing.** What the guest can see is checked.

**What is NOT to be measured any more — so that nobody plans it a third time.** The obvious
halving "a store to `0xfee00008` from the kernel" **has been run, twice, and cannot decide**:

* In **remappable** form it measures nothing under QEMU: interrupt remapping hangs there as a
  memory region in the **device** address space; a CPU store goes past it.
* In general a store there is no message dispatch, but a **register access** to the
  own LAPIC page — and under x2APIC the page is switched off. A processor sends itself a
  vector via **ICR**, not via store.

What has taken their place and **carries** is the **self-IPI** (`msi_zustellprobe`): vector
`0x64` reaches `irq_hook`, is drained and arrives at the notification. Thereby the second
half — APIC → IDT → dispatch → `irq_hook` → drain → notification, including binding and badge —
is **proved**, and the halving is no longer open: the error lies before the IOMMU.

**The two device-side first candidates are likewise already excluded:**
`VRING_AVAIL_F_NO_INTERRUPT` is **not** set (`queue_setup` zeroes `avail.flags`, `publish`
does not touch them), and bus master is **on** — `used.idx=1` would otherwise be impossible, because without BME
the device would never have read the descriptor.

**What is to be done belongs to the MEASUREMENT RIG:** three runs that treat the environment as a variable —
without KVM (TCG), without `intremap`, with `eim=on`. Only when the environment drops out is the kernel
a suspect again.

**Not to be done:** a poll fallback in the driver. It would undermine `poll-runden == 0`, i.e. exactly the
conjuncts that B4 carries — the line would stay green and measure something different (plan §4).

**The open edge, named:** `WAIT` has no deadline (stage A is missing). With the wait path switched on
the driver blocks for ever and **takes the suite along** — that is no longer a forecast value,
but an incident of 2026-08-26. The interim version is a deadline of the **measurer** (timeout
in the test driver, not in the ABI); it does not anticipate stage A.

---

## D16. Fixed numbers next to derived lengths — the fourth occasion

**Class:** structure · **State:** three individual fixes, no sweep.

Three times the same form, each time expensive, each time covered for years by a size relation:

* `dense = [first; 8]` in `loader.rs` next to a derived `slots` — with the ninth
  endowment slot it wrote beyond its end;
* `STACK_CAP_SLOTS = 1024` next to a thread table dimensioned at boot time;
* `CAP_BUDGET_PER_PD = 8` next to what the loader grants — the promise was unfulfillable, and the
  rejection did not carry the reason.

Two of them ran on one architecture only through luck. At the fourth occasion a
**systematic sweep** is cheaper than the next individual fix: check array literals and
capacity constants against the lengths from which they follow.

The construction already stands there (`const _: () = assert!(LOAD_CAPS_MAX <= CAP_BUDGET_PER_PD, ..)`); what is open
is only at how many places it is still missing.

---

## E-T1. `FSGSBASE`: a gain that makes an option more expensive

**Class:** decision · **State:** open, **deliberately not** waved through as an optimisation.

`WRMSR` **serialises** (over a hundred cycles), `WRFSBASE` is a few. With one write per
context switch the difference is real; the precondition is a CPUID bit plus `CR4.FSGSBASE`.

**The catch is no implementation question:** `CR4.FSGSBASE` also releases **`wrgsbase` from ring 3**.
As long as `GS` is unused, without consequence -- and that it **stays** unused was the by-product of the
TLS decision (`FS` and `GS` are both free, `FS` was chosen). As soon as `GS` is to become the
per-CPU store, the GS base at kernel entry is a **user-chosen** value, and the `swapgs` path must withstand that. Linux rebuilt its entry path for it.

The gain is real, the price is an option whose value nobody knows today. That is why this stands
here and not in a commit.

---

## A2. Deadlines — the remainder (2026-08-28)

**Class:** kernel primitive · **State:** `[~]` — `WAIT` measured and cross-checked (see `done.md`),
**three** pieces open (A2n has been closed since 2026-08-28). **Kept separately, because they are of different
difficulty.** Combined in one item, the most expensive hid behind the cheapest.

**A2 (remainder). `CALL` and `PARK` carry no deadline.** Built is exactly one call site of
`frist_setzen` -- the one in the `WAIT` arm. The case that justifies A2 at all (a driver waits for
a device that does not answer) runs via `WAIT`, which is why it stands there first; `CALL` is
the next, because a client at a dead server is in the same situation. **Caution with `CALL`:** a
reply duty hangs on it -- an expired deadline must not leave the server with a
reply right on a thread that is no longer waiting. Exactly the form that produced the `ERR_EP_FULL` finding at `WAIT`, only with a second party.

**A2n is DONE** (2026-08-28, `tools/fristen-negativ.sh`, 4 of 4 isolating) -- see `done.md`.
What REMAINS open of it is one thing: **the overflow of `FRISTEN_JE_TICK` is read, not
driven.** It needs seventeen threads with deadlines in the same tick; the probe has one. The error
in it (reason removed, thread neither enqueued nor reported -- i.e. *lost* instead of *delayed*,
against the promise in the comment beside it) is fixed and **unguarded**.

**A2c. The race is decided, but not hit.** The resolution stands (*the signal
wins*) and is implemented; the probe however signals **before** waiting, thus hits the
window only in the one, easy situation. The document expressly demands to hit it
**deliberately** -- signal and timer in the same tick. Without this measurement "the signal wins" is a
statement about the source text, not about the run.

**A2v. The Verus effort — the only unpriced quantity of the plan.**
The model-fidelity guard reported the three places by itself; they stand as **outside
with debt** in `tools/verus-modelltreue-sched.sh`:

```
'frist':           'absolute Frist in Ticks -- das Modell kennt keine Zeit (A2, SCHULD: todo A2v)'
'frist_grund':     'dito -- der Grund, den die Frist entfernen darf'
'fristen_faellig': 'A2: Wecker mit Zeit-Ausloeser. Effekt = unpark, Ausloeser nicht modellierbar'
```

**The effect is modellable, the trigger is not.** `fristen_faellig` does what the model knows:
remove exactly one reason, enqueue on an empty set -- literally `unpark`. What is missing is the
**time**. To enter it as a partner of `unblock` would be the convenient version and a claim:
the proof could not check that the deadline fires **when it should**, but only that it
wakes correctly **when it fires**. That would leave exactly the two failure pictures unproved that make A2
dangerous -- a deadline that fires too early (the corruption path), and one that never
fires (the hang, against which A2 is built).

**The honest cut is therefore to extend the model by a clock**, not to enter the site.
What that costs, this entry does not know -- and exactly that has stood in the document since the first
draft: *"The Verus effort is the only unpriced quantity."* It has remained
unpriced after A2; what is new is only that it is now named **what for**.

---

## D18. Checkers that CANNOT fail — the class that becomes more credible with every run

**Class:** checking system · **State:** **ten** instances known, no sweep. **Not D16.**

The difference in the failure picture decides: a wrong number (D16) turns red sooner or later. A checker
that **cannot fail** reports green permanently — and the longer it does that, the more
trust it carries. It is not discovered, it is quoted.

Ten instances, all proved:

| Case | what looked like a check |
|---|---|
| `ueberlappung-abgewiesen` | the checker read `KSTACKS.ubase_of`, which the spawn path deliberately never fills — **structurally blind** |
| `SYS_SPAWN` | completely built, never run, kept in the register as *missing* |
| `dma_audit = 2` | number printed, never compared — a 3 too would have been silent |
| **M5** (2026-08-26) | the `sed` pattern hit nothing any more after a rework; the negative case was silently switched off |
| **`b4-aktiv`** (prevented) | a premise that never becomes true is a conjunct that never judges |
| **`getrennt`** (2026-08-27) | compared two addresses that the CHECKER had chosen -- if a thread fell silent, `0` stood there, and `0 != b0` read as "separated". **True because nothing happened** |
| **`konjunkt()`** (2026-08-27) | the extractor of the counter-proof read the **wrong line** (`irtevgb` has a `getrennt=true` of the same name, `head -1` took its value) -- a false alarm for two runs |
| **M2, first version** (2026-08-27) | the mutation destroyed something **harmless**; a counter-proof that breaks nothing proves nothing |
| **`nicht-zu-frueh`** (2026-08-28) | read the span that the EL0 thread put around its `WAIT` -- i.e. deadline **plus scheduling**, and on `IDLE_PRIO` the scheduling is the larger summand (240 ms for a 100 ms deadline). A deadline shortened by **one** tick left it green: "too early" was structurally invisible |
| **`lief-nicht`** (2026-08-28) | measured a window of five ticks in which an `IDLE_PRIO` thread CANNOT get its turn at all -- green, because the observed had not yet run |

**The two of 2026-08-28 are the most instructive, because they arose TWO DAYS after this entry**
-- in the probe whose counter-proof then found them. Both have the same root, and it is
written down twice already in this file: **a quantity that the measured path co-determines.** The
EL0 thread runs on `IDLE_PRIO`; every time span that *it* puts around an operation contains its own scheduling, and that dominates. The finding stood thirty lines further
up in the same file -- the A1 half lists the 140 ms of lagging expressly as a result.
**Measured now is kernel-side, in ticks.**

**The remedy is the same in both cases: check the PREMISE, not only the conclusion.**
The hit count in the mutation tool and M8 are two instances of it — the one checks that the
mutation takes hold, the other that the condition becomes true even once.

**The extractor belongs to it.** "Hits exactly once" protects the MUTATION; that the **reading**
sits on the intended line is a second question, and it cost two runs on 2026-08-27.
Both negative scripts have since anchored on their own report line (`ZEILE=`).

**The open question, and it is answerable:** *which conjuncts in the tree have no counter-proof
that turns them red?* For `irqmsi` it is, after 2026-08-26, eight of eight. For the older lines
(`dmaiso`, `pdcolor`, `ladepol`, `cycacct`, `epfull`, `verif`, `arena`, `dmapool`, …) the number is
**unknown** — and "unknown" here is not neutral, but the precursor of the five cases above.

A sweep counts first and mutates afterwards; the count alone is already a statement.

**Supplement to the hit check:** "hits exactly once" does not exclude that the one hit
sits at the **wrong place** after a rework. Only the isolation column protects against that — the
remaining conjuncts must stay green —, and it must be evaluated **per mutation**, not
only the intended line. In `tools/irqmsi-negativ.sh` that is the case for all eight.

---

## D17. A kernel panic is a measurement blind spot

**Class:** diagnostics · **State:** open, own decision.

On 2026-08-26 `load_program_into_pd` panicked in an array overflow. Because a panic does not
take the node down (B-6.2: the panic path holds the core **without** IRQ masking, the next tick
brings it back), from outside it looked like this: **four of six programs were missing**, threads
created and never admitted, no watchdog, no line. The picture was silence.

Whether B-6.2 should stay so is a question of its own. That a panic becomes visible **somewhere** — a
counter, a line in the report, a conjunct — is not.

---

## C7b. The 2 MiB per isolated PD are measured and lowered — and the lever was the wrong one

**Class:** capacity / product goal · **State:** the measurement stands, the size is lowered
(2 MiB → **64 KiB**), the curve is **re-measured** — and it refutes the "factor 22".
Open is the stock that now binds (see C7c).

Tenant density hangs on this number (`RAM / region size`, see C7), and it was so far
a sum without summands. Read in the source, not estimated:

| Item | before | **now** | what it is |
|---|---|---|---|
| **private region** | **2048 KiB** | **64 KiB** | **the EL0 stack of ONE thread**, `mem_alloc_anywhere`, zeroed with `zero_phys`, mapped to `SLOT_DATA` |
| page tables | 20 KiB | 24 KiB | +1 frame: without the 2 MiB block the user window needs a table |
| EL1 kernel stack | 4 KiB | 4 KiB | since 2026-08-12; before 16 |
| guard page | 4 KiB | 4 KiB | since 2026-08-10 |
| | **≈ 2076 KiB** | **≈ 96 KiB** | **measured** in the curve ("per process it cost 96 KiB"), not computed |

**The finding was right: the 2 MiB were GRANULARITY, not need.** `vspace_map_user_window`
has two branches — `len == TWO_MIB && phys % TWO_MIB == 0` maps **one 2 MiB block** (no
page-table frame), everything else page by page **with** a table.

**The measurement that was missing** (2026-08-13, x86, check line `ustack`, gates):

| Suite | deepest measured EL0 path | Region |
|---|---|---|
| `test-qemu-x86.sh` | 6168 B — and that is the **depth probe itself**; the workers of this suite do not touch their stack (15 of 17 measurements: depth 0) | 2 MiB |
| `test-qemu-x86-load.sh` | **7400 B of 16384 B (45.1 %)** — a real loaded program | 16 KiB |

Sum condition, as with C4 and not at the peak: `7400 B + 8192 B reserve = 15,592 B ≤
65,536 B`, distance factor 4.2. **The second summand is demonstrably ZERO on EL0** — a trap from
ring 3 always switches the stack (x86 `RSP0` from the TSS, aarch64 `SP_EL1`); the counterpart to the
interrupt handler on the same EL1 stack does not exist here, and signal handlers on the
user stack do not exist in this system.

**AND NOW THE RESULT THAT TURNS THE ENTRY AROUND.** The curve was re-measured — both states
with the **same** kernel, only the constant swapped:

| RAM | before (2 MiB) | Reason | **after (64 KiB)** | Reason |
|---|---|---|---|---|
| 512M | 220 | **RAM** (1 MiB free) | **321** (+46 %) | **guard table pot**, 419 MiB free |
| 3G | 1119 | **guard table pot**, 739 MiB free | **979** (−12.5 %) | **guard table pot**, 2918 MiB free |

**"Factor 22 on the tenant density" was false, and for a reason that the calculation could not
see:** it computed `RAM / region size` and thereby presupposed that RAM binds.
At 3 GiB it did **not even before** — there the fixed
stock of split guard page tables was already the bound with 2 MiB regions (1119 PDs at 739 MiB free RAM). The
**cost** per PD really fell by factor 21.7 (2076 → 96 KiB, measured); the **density**
did not follow, because the next pot is fixed. The gain stands where RAM really
bound: at 512 MiB, +46 %.

That the number at 3G even **falls** (1119 → 979) is no contradiction, but the same cause:
the pot binds at the number of **distinct 2 MiB blocks** over which the kernel stacks scatter,
and that hangs on the stride per PD. Whoever changes the region size changes the scatter.

- [x] **The measurement: how deep is the EL0 stack really?** Stands as `kernel/src/userstackmark.rs`
      (zeroing instead of fill pattern — the region is zeroed anyway, the highest
      non-zero byte IS the water mark), with calibration (three cases) **and** a depth probe
      on the real path. The probe checks what the calibration structurally cannot: that the
      bookkeeping thread slot → region points to the **right** region. An entry that points to
      some other zeroed region reports "plenty of air" and would pass every calibration.
- [x] **Lower the size**, with the sum condition. `hal::mmu::PRIV_REGION_SIZE = 64 KiB`.
- [ ] **The second way stays standing beside it and is now the ONLY one that still moves anything:**
      leave the VA at 2 MiB and back it **lazily** (memory server, [Z16](#z16-translate-source-text-instead-of-running-binaries--assessed-2026-08-09) stage 1).
      Then a PD costs only what it touches. **But:** as long as C7c binds, even that does not change
      the density — the order is thereby reversed, C7c comes first.
- [ ] **What the measurement does NOT see, and it belongs in every follow-up decision:** a stack word written with
      **zero** is invisible (the pattern method of `kstackmark` detects every
      write access, the zero method only those with value ≠ 0). The measured value is a
      **lower bound**; the distance factor 4.2 to the region size is the answer to that. Whoever wants to lower the
      size further first needs a method without this blind spot — such as
      the saved user `RSP` in the trap frame as a second, content-independent
      estimator.
- [ ] **The `ustack` report stands only in the x86 boot-up path**, the counters run along arch-neutrally.
      The same classification (and the same gap) as with `kstack` and `sperre` — see C9d.

## C7c. The stock that binds NOW: 16 split guard blocks (measured 2026-08-13)

**Class:** capacity / product goal · **State:** measured and named, not fixed.
**It has been the bound of tenant density since C7b** — on every RAM size that was checked.

`hal::mmu::GUARD_BLOCKS = 16`. A kernel stack lies above 16 MiB, where the identity map stands in
2 MiB blocks; a 4 KiB guard demands splitting the block into a page table, and
the HAL has no allocator (kernel boundary) — the stock is static. If it does not suffice, the
stack request is **rejected by name** (`MANGEL_GUARD_TABELLE`) instead of being served unguarded. That is
right so; the bound is fail-closed and no correctness gap.

**The derivation of the 16 no longer holds — and it never computed the right thing.** In the source
it says: "a 16 KiB stack with guard occupies 20 KiB, so well over 100 of them fit into a 2 MiB block,
16 blocks carry about 1600 guarded stacks — more than the 224..1477 of an isolated
capacity curve". Two errors in it:

1. The EL1 stack has been **4 KiB since 2026-08-12**, with guard thus 8 KiB — 256 per block, 4096
   in total. The number in the comment is outdated.
2. More important: the calculation measures the **packing density within** a block. What is bound however is the number
   of **distinct** blocks over which the stacks scatter — and that hangs on what
   else is allocated between two stacks. Measured at 512M/64 KiB: **321 guarded stacks over
   16 blocks = about 20 per block**, i.e. **7.8 % of the 4096 that would fit in**.

**The structural answer already stands in the source and can now be quantified:** a
**stack arena** — a contiguous area whose blocks are split once. It raises
the density by the factor that stands above as waste (about 12), without a single static
table more.

- [ ] **Stack arena instead of scattered kernel stacks.** Rework of the stack allocation, not a
      larger constant. Raising `GUARD_BLOCKS` buys linear numbers with linear BSS and
      leaves the waste of 92 % standing.
- [ ] **Until then the number belongs in the product statement**, not the calculation `RAM / 96 KiB`:
      measured are **321 (512M)** and **979 (3G)** isolated, **live** tenants.

## C9. Lock hold time — the mark stands, the three findings are open (2026-08-12)

The **lock hold time mark** has measured since 2026-08-12 how long the kernel runs in one stretch with
masked interrupts (`crates/caprock-sync`, `kernel/src/sperrmark.rs`, check line
`sperre`). The threshold is derived and not felt: **one timer tick** — masked longer
means demonstrably lost preemption. Counter-proof: `tools/sperr-gegenprobe.sh`.

**The first run found three places that had been a number nowhere before.** All three
stand as a *named, capped* debt in `sperrmark::SCHULDEN` (ratchet per item, may only
fall) — they are thereby visible, but not fixed. The order is the point: **each
became visible only when the one before was named** — a maximum conceals what is smaller.

* [ ] **C9a — `kernel/src/colors.rs:982`: 1.19 billion cycles (about 420 ms, 42 ticks).**
  `run_prime_probe` (B-4.5) takes `PP_ARENA.lock()` and releases it 162 lines later;
  in between lies the *whole* measurement — `system::alloc` (i.e. the MEM lock **under** the
  arena lock), the search for colour-pure runs, thousands of pointer-chase runs, the return. The same
  form as the C8 finding, only without `while let`: it is not the lock that is too long, the **work lies
  underneath**. Check code, no production path — hence (a) and not (c).
  *Way:* the arena needs exclusivity, not IRQ masking. Either lift the measurement out of the
  critical section or a primitive without masking. Needs its own counter-proof.
* [ ] **C9c — `kernel/src/system.rs:9198`: 20.4 million cycles (7.3 ms, 0.73 ticks).**
  `purge_ipc_for_thread` holds `IPC_ORPHANS` as an **outer** lock over a sweep of
  O(endpoints + notifications) = 20,128 individual lockings — **per thread death**. The
  *iteration count* had stood in the comment there since C4/D10; what stood nowhere is the
  **latency** that follows from it. Pure production path, and already today at three quarters of the threshold
  (measured 494…748 per mille over five runs). Cap therefore **tighter than the threshold**
  (856 instead of 1000 per mille) — it is a shackle, not a free pass.

* [ ] **C9d — the mark gates only on x86.** The calibration runs on both architectures (and
  passes there too, `0b111111`), the **report line and the gate** stand only in the
  x86 boot-up path — the same classification as `kstackmark`. Two reasons, both nameable: the
  debt list contains with `console.rs` an **x86 path** (aarch64 has a different console
  driver whose number nobody has measured). The **second** reason has dropped away since 2026-08-13:
  the aarch64 suite is green again (see C9e). A gate there is thus acceptable,
  and what is still missing is only the aarch64 console number.

* [~] **C9e — FIXED on 2026-08-13; what remains open is the class, not the case.**
  The picture was `color : FAILURES` with **all zeros** (`in_mask=0 kernelseite=0 disjunkt=0
  bilanz=0`, with `uebergross_abgewiesen=1`), afterwards watchdog with `offen: color` — 3 of 3
  runs character-identical, hence deterministic and not D13.

  **The cause was a size relation, not concurrency.** aarch64 has 16 colours, with
  `PARTITIONS = 4` a stripe is thus **4** colours wide; an EL0 kernel stack is there
  16 KiB = **4** pages and thus fitted *exactly* — the property "the stack fits into its
  stripe" followed from `stack pages == stripe width` and not from the structure. Since the
  guard page (2026-08-10, `USER_KSTACK_ALLOC = USER_KSTACK_SIZE + PAGE`)
  `claim_user_kstack_masked` demands **5** consecutive pages. Five consecutive
  pages carry five **different** colours; into a 4-colour stripe they fit at no
  address and at no fill level. `alloc_colored` thus **always** gave `None`,
  `spawn_isolated_colored` likewise, and `run_color` fell out before the `if let` — hence exactly
  the fields zero that arise behind the `if let`, and `uebergross_abgewiesen` as the only one
  at 1.

  Two circumstances that kept it invisible: on **x86** a stripe is 16 live mask bits
  wide and the stack since C4 two pages — there is plenty of air. And on **aarch64 the guard
  does not exist at all** (`hal::mmu::guard_unterstuetzt() == false`, `guard_unmap` is a
  counter): the page that made every coloured stack request impossible demonstrably does **nothing** on this
  architecture.

  **Fixed** by `Allocator::alloc_colored_vorspann_in`: the first `n` pages of a
  coloured request stand under no colour condition. That is no weakening — a
  guard page carries no data and occupies no cache set; the colour assurance says nothing
  about it. In addition the alignment of `al` (16 KiB) lowered to `PAGE`: with 16 KiB alignment
  of the **block** the stack at `roh + PAGE` would precisely *not* have been stripe-aligned.
  Proved without QEMU (`caprock-mem`, `wachseite_sprengt_den_streifen_bei_16_farben`: 4 pages
  fit, 5 never, with lead-in again — and every stack page in the stripe), counter-proof in the kernel
  isolated to **one** parameter (`vorspann` 1 → 0 restores the picture character-identically,
  every other line stays green).

  **What remains open:**
  - [ ] A coloured request that is **wider than the stripe** is structurally
        unfulfillable and nevertheless comes back as `None` — indistinguishable from "memory
        exhausted". Exactly that is what this search hung on: the failure picture looked like shortage.
        What is asked is a reason of its own (`MANGEL_*` does not suffice: it is no shortage).
  - [ ] `colors::region_bytes()` already knows the bound (`stripe width * PAGE`) —
        `USER_KSTACK_ALLOC` checks against it nowhere. Two quantities that must fit
        each other, and not a single line that says so.

---

## READING AID: what which identifier means — and the state of the four Z26 addenda

**One identifier, two meanings** (noticed 2026-08-10, before it became a footnote):

| Identifier | in **Z19** (substrate of the language runtimes) | in **Z26** (GPU/CUDA) |
|---|---|---|
| `A1` | the process start stack (`argc`, `auxv`) | port the NVIDIA **kernel module** |
| `A2` | TLS | the `/dev/nvidia*` **ioctl surface** as a PD protocol |
| `A3` | **stack and guard page** | the **binary Linux ABI** (loader for foreign ELFs, signals, `futex`, `mmap`) |
| `A4` | enable SSE (done) | deliver 84 MB of **GSP firmware** + attestation |

Whoever says "A3" without a strand has meant two different works. **From here on always with strand:
`Z19/A3` or `Z26/A3`.** The same sort of drift as "seL4 → SEL4 → Sel4lake", only with identifiers
instead of names — and expensive for the same reason: in three months nobody reconstructs it any more.

**What was built on 2026-08-10 is `Z26/A3` — more precisely: only its core**, the
kernel primitive for redirected syscalls. Not the loader for foreign ELFs, not the
glibc symbol surface, not signals/`futex`/`mmap`.

**State of the four addenda that were to be clarified before the build:**

| | Addendum | State |
|---|---|---|
| 1 | The reversal of the recorded decision **Z16** must be named as a new decision | **done** — stands as an entry; the demand of the old analysis (WSL1 failed on **behavioural fidelity**) is taken over with it |
| 2 | The **authority list** was register-sized: frame write scope, guest **memory access**, **vspace** right | **open** — by its own measurement the primitive grants **1.5 of 4**. Without guest memory access *every syscall with a pointer* is not implementable (`read`, `write`, `ioctl` — for CUDA the whole surface); without vspace manipulation no `mmap` |
| 3 | The **blocking seam** (cycle prohibition **in the kernel**, wait reason in the reason set) | **done, both halves** — `pruefe_bindung` walks the graph (not only the direct partner), wait reason in `BlockReasons` from day one |
| 4 | **Measurement matrix with a threshold fixed in advance** | **half** — the threshold stands (redirected `getpid` ≤ 2000 cycles, anchored at Z18 (2)), **nothing is measured**. For `read` 4/64 KiB no threshold can be fixed, because the path does not exist (guest memory authority missing, see addendum 2) |

**The biggest gap of the primitive was closed on 2026-08-13:** the **payload** stands. The
frame is copied into the sidecar and the result written back, a path mints handler caps, and
`SYS_SETHANDLER` has been successful — proved as `redirect : ALL PASS` on a real guest
that makes a redirected syscall and **sees the result**. Details, counter-proofs and the
**new** open items (isolated handler / EL0 guest, `rip`/`rsp` in the write-back) below at
[A3](#a3--the-kernel-primitive-is-built-2026-08-10-what-stands-what-is-open-and-the-threshold).

## What the sweep through the one-sided comparisons did NOT cover

**Class:** checker form · **State:** the sweep itself is done (see done.md),
these remainders are not.

The rule stands: *every measured quantity that goes into a comparison with **only one** bound
needs a plausibility lower bound — or the value zero must expressly drop out as "not measured".*
One live site was found and fixed (F1). Two classes the sweep **did not**
reach, and a sweep that conceals its reach is the next null statement:

- [ ] **Checkers that do not even collect a quantity.** The sweep searched for comparisons;
      it can say nothing about lines that never form a number. That is the class "a test
      that runs nowhere", only one level deeper.
- [ ] **Comparisons inside the Verus models.** The same forms apply there, but the
      search patterns of the sweep (shell comparisons, Rust verdict lines) do not take hold.

## S2. The second SPARK data point: the scheduler — and it does NOT break

**Class:** verification / security · **State:** question answered, **five findings in the Rust code
open**

The decision question was: *how many percent of the scheduler core come under `SPARK_Mode => On`?*
The cap space came with 34/34. The scheduler has what that one did not: **concurrency, time and
the `Parked` type**.

**Answer: 65 of 68 subprograms analysed. Skipped exactly three — and all three are
bodies that do not stand in this source at all** (imported foreign-crate function, a
`black_box` stack address that is provable in *no* language, and a run-time body).
**Not a piece of scheduler logic under `Off`** — ready queue, reason set, budget/refill,
`spawn_parked`/`admit`, zombie bookkeeping, migration, audit: everything in.

**So it does not break at 60 %, and the language is not the reason.**

**The proof rate is nevertheless clearly worse than with the cap space: 38 % open against 15 %.**
And the reason is design, not language — **raw machine indices at every seam**. That is
the actual statement of this data point.

### The three questions

**Concurrency carries** — 63/63 data dependencies proved: the Rust code reads everywhere exactly
once into a copy, and that is now **proved instead of read**. What does *not* work: "the caller
holds the spinlock" has **no form of expression** in SPARK — it remains a comment in both languages.

**RE-MEASURED 2026-08-13: in VERUS it has one.** Rebuilt at `kernel/src/system.rs:310`
(`record_user_kstack`, a comment today): a `tracked struct SchedsGuard` with a private field,
`lock()` as the only source, `requires` at the choke point. Three callers, three **different**
outcomes — correct core `verified`; **foreign** core `precondition not satisfied`; self-built
evidence `constructor for an opaque datatype`. Under `#![no_std]`, and `tracked`/`ghost` is erased before
code generation: **not a byte, no heap.** Costs: 9 lines of witness type, 8 lines of
lock contracts, 1 `requires`, 1 argument per call site.

**And `Parked`: YES AND NO.** `tracked` is **affine, not linear** — a function that drops
the witness passes (2 verified, 0 errors). An automatic leak check like SPARK's "leak
proved" does **not** exist. With a ghost balance it works after all and without a heap (`spawn_parked` raises
`offen`, `admit` lowers it; `vergessen()` → `postcondition not satisfied`, `ordentlich()` →
verified) — but that is a duty that one must **write down**, through every signature.
**That remains SPARK's only real lead**, and in Verus it costs a balance instead of a
switch.

**Time is the weakest class** — `now += 1` formally open (at 100 Hz after 5.8 billion years).

**`Parked`: SPARK can do it better — error instead of warning.** Measured on two subprograms that
differ in *one* line: with `Admit` → "absence of resource or memory leak **proved**",
without → "leak might occur". Rust's `#[must_use]` is a **switch-off-able warning**.
**The price, and it weighs here:** SPARK's linearity hangs on an **allocation**, Rust's `Parked`
is a **stack value** — for a kernel without a heap a real objection.

### Five findings in the Rust code

- [ ] **S2a — `migration_candidate` walks the ready chain WITHOUT a step bound.** Re-checked
      (`crates/caprock-sched/src/lib.rs`): `while i != NIL { … i = t.qnext … }`, while `audit`
      over the **same** chain carries `if n > q.count { return 5 }`. Runs in **load balancing under
      the core lock**: a cycle there is a standing core. The same form as B-5.5, only
      the other way round — there the *checker* was bounded and the path was not.
- [ ] **S2b — `depleted_count` has no audit recount.** Code 10 exists only for
      `budget_blocked_count`. And it is **exactly the counter that lied in D8/M5** — four
      decrements, two increments, all without helper.
- [ ] **S2c — `debug_assert_eq!(core, self.core)` stands 15× in `lib.rs` and 0× in release.**
      All eight open assurances are this form: a promise that does not exist in the delivered kernel.
- [ ] **S2d — `priority: u8` indexes `[ListHead; 8]` raw**, and `NPRIO` occurs outside the
      crate at **exactly one** place. Executable counter-proof: prio 0 ✓, prio 7 ✓,
      **prio 8 → `CONSTRAINT_ERROR`**. In Rust a panic with `panic = "abort"`.
- [ ] **S2e — the Verus scheduler model carries `blocked: bool`, the state that [Z24](#z24)
      ABOLISHED.** 7 modelled fields against 23, arithmetic only `nat`/`int`. The
      model-fidelity guard checks the *mapping*, not the *reach* — the same gap as with
      S1.

### A trap at the own tool

**gnatprove collects per object directory.** After the project split the S1 run reported
**232 instead of 99** checks. The ratchet tore — **in the opposite direction it would have been silent**.
Fixed (clearing + unit check); as a trap in `CLAUDE.md`.

## S1. The SPARK experiment found two real errors (2026-08-13, branch `spark-experiment`)

**Class:** verification / security · **State:** experiment answered, **two findings in the
Rust code open**

The question was: *does GNATprove at Silver level find something in the cap space that the
Verus model does not see?* **Answer: yes, 15 places.** The key figure that decides whether that
weighs: **34 of 34 subprograms under `SPARK_Mode => On`, not a single `Off`** — 294
checks, 279 proved, 15 open (5 %). The first run had 39; the difference of 24 was missing
loop invariants of my port, removed by assertions that GNATprove **proves
itself**.

- [ ] **S1a — `audit_cdt` can take the node along instead of reporting the anomaly.**
      RE-CHECKED in the real source (`crates/caprock-cap/src/space.rs`, sibling walk): `p`
      is checked against `nslots`, **`first_child` and `next_sibling` are not** —
      `self.slots[ci]` indexes unchecked. The step bound (B-5.5) protects against **cycles**,
      not against an index **outside the table**.
      In Rust that is a panic, and `panic = "abort"` stands in **both** profiles: **the checker
      that is to report the anomaly takes the node along.** The same form as B-6.2, only in the
      audit path.
      The counter-proof isolates the **position**, not the falsification — the same broken value,
      only the slot moves: clean → `audit_cdt = 0`; broken at slot 0 → `6`; broken at slot 1 →
      **`CONSTRAINT_ERROR`, no verdict**. The check for `p` stands in its *own* iteration;
      if `p > s`, it has not yet taken place.

- [ ] **S1b — `refcount -= 1` in `delete_leaf` without a condition, and `overflow-checks` is NOT set in
      release.** So no panic, but a **silent wrap to `0xFFFF_FFFF`**:
      object never finalised, region never freed, reply never aborted. The same form as
      D11 — a bound that nobody names is a hole instead of a protection.

### Why Verus cannot see a single one of them — counted, not claimed

`refcount` stands only in `cap_cdt_refcount.rs` (21×, as **`nat`** — a `nat` model cannot even
pose the question about `refcount -= 1`), `parent` only in the structure files. **No file
carries both in the same state.** `gen`, `Finalized`, `rights`, `badge`, `move_cap` do **not occur at all** in
the whole model. `delete_leaf` lowers the refcount **and** unhooks — this
compound operation is proved nowhere.

**That joins the register corrections:** "16 proof files, 0 errors" says that the
existing proofs hold — about the **reach** it says nothing, and here it is measured.

### Four unwritten assurances that the port brought to light

**I5** length of the sibling chain (Verus has *no* length or termination statement for it) ·
**I6** the capacity promise of `Finalized` · **I7** the **intentional** wrap of the generations —
`resolve` rests on it and thereby the **entire** handle safety, and generations do not occur in the
model · **I9** a window in the `install` rollback (`used = false` at `refcount = 1`)
that `audit_cdt` structurally does **not** report: code 3 compares for unoccupied objects only `refs`,
not `refcount`. Harmless, because `alloc_object_inner` overwrites — **a coincidence of the order,
no design.**

### What that says about SPARK — and what not

The gain did **not** come from "Ada is safer", but from GNATprove treating **every**
indexing and **every** arithmetic as a proof obligation, while Verus proves only what
someone has modelled. That is an argument for **more proof surface**, not necessarily for a
change of language.

### RE-MEASURED 2026-08-13: Verus finds S1a AND S1b on the real code — the conjecture holds

The counter-thesis is **confirmed: it was the DEFAULT, not the language.** With
`#[verifier::verify]` on the `mod` line (resp. `--no-external-by-default`) every indexing and
every arithmetic in ordinary Rust is a proof obligation, as with GNATprove.

| | Report |
|---|---|
| **S1b** `space.rs:1067  refcount -= 1` | `possible arithmetic underflow/overflow` **and** `precondition not satisfied` — body unchanged |
| **S1a** `space.rs:844  self.slots[ci]` | `precondition not satisfied`; the `self.slots[p]` one line above is **discharged** — the reporter is discriminating, not blanket |

**The number on which the comparison hangs are two different quantities** — the mix-up would be the
error:

* **RAISING obligations: 0 lines per line of code.** A switch. Delta to the production text over the
  whole file: **24 lines, of which 21 attribute lines and 3 `derive` — not a single function body.**
* **DISCHARGING obligations, measured at `delete_leaf`: 12 lines of specification on 19 lines of code
  (0.63).** Afterwards **exactly one** obligation remains open — S1b. Lower bound: two caller contracts
  are `external_body`, i.e. assumed instead of proved.

Run A (whole file without `audit_cdt`): 48 of 67 functions without an open obligation, 60 open obligations
(54 index bounds, 6 arithmetic). Run B (+ `audit_cdt`): 90 — the 33 additional lines there are
**exclusively loop form** (`for` → `while`), no condition and no index changed.

- [ ] **S1c — the overflow counterpart to S1b that SPARK did NOT name:**
      `crates/caprock-cap/src/space.rs:543  self.objects[obj].refcount += 1;` in `copy`. The same
      missing bound in the other direction. Covered in practice, because the refcount stays far below 2³² through the
      boot-dimensioned slot table — **but S1b was also "covered in practice", until it was not.**
      A bound that follows from a size relation instead of from the structure disappears at the next measured value.
- [ ] **`space.rs:170  &self.dma[..self.dn]`** in `Finalized::dma_regions` — holds, because `push_dma`
      carries `dn <= len` along. **That stands nowhere.** An assurance that nobody writes down is
      gone at the next rework.
- [ ] **I5 is a gap of the MODEL, not of the tool.** Verus demands `decreases` for **every**
      loop — strictly stronger than SPARK Silver, which does not demand termination at all. The
      termination statement about the sibling chain was thus not missing because Verus cannot do it,
      but because the model did not use it. The same form as "16 proof files, 0 errors"
      says nothing about the new property.
- [ ] **What stands in the way of the switch is TOOL MATURITY, not expressive power** — and that is
      the item that would have to be quantified before anybody puts `space.rs` permanently under Verus:
      four reproduced crashes in Verus `0.2026.06.20` (the worst: `for i in 0..n { c[i] }`
      **outside** a `verus!` block, i.e. exactly on the path that gives the GNATprove attitude);
      `IndexSetTrustedSpec` in vstd sealed, which is why a **own** container `x[i] = v` cannot give a precondition
      (worked around via a `Deref` newtype on `Vec`); no
      vstd specifications for iterator adapters (`position`, `filter`, `count`, `copied`,
      `enumerate`) — for that reason alone **6** functions of `space.rs` are unreachable;
      `#[derive(Debug)]` unverifiable and `#[derive(PartialEq)]` **fails**.

**Tool chain:** `alr install gnatprove` + `gnat_native`, entirely under `~/.alire`, without
root. `tools/spark-beweis.sh` is a **gate** (ratchet in both directions, coverage check
against secret `SPARK_Mode => Off`, too few checks count as a **failed** run) and
caught an error of its own on the first run.

## S4. `kstack` — the gate judges EARLIER than the report (2026-08-14)

`crate::kstackmark::urteil()` is called **twice**: `bringup.rs:4345` in the **gate** (the list
that decides `== ALL PASS ==`) and `bringup.rs:4824` in the **report line**. In between lie
about 480 lines of report — and the measured quantities `TIEFE_MAX`/`FREI_MIN` are `fetch_max`/
`fetch_min`, i.e. **monotonic**: every further stack use can only make them worse.

**Thereby a run is possible that prints `kstack : FAILURES` and nevertheless reports `== ALL PASS ==`.**
The opposite direction is structurally excluded (the marks never get better), so it is exactly
the dangerous half: **the verdict is milder than the measurement.**

The same class as two already paid-for traps, only reversed: "a verdict that stands in
`all_done()` must not arise only in the report" — here the **measurement** arises after the
verdict. And "**WHOEVER MEASURES SETS THE MARK** — the measured path may neither set nor delete it":
the report path uses the same stack that it measures.

- [ ] **Read from the source text, NOT run.** Whether the difference ever occurs is open — the
      reserve is generous. **That changes nothing about the form:** a gate that judges earlier than
      its measurement is no gate, but a snapshot.
- [ ] **Check whether `ustack` has the same form** (the finding names both).
- [ ] The obvious fix — measure **once**, hold the value, feed gate and report from the
      **same** reading — is the same as with `drv : ALL PASS` ("a value is captured
      where the statement holds, not where it is printed").

## S3. Unprotected subtractions — 18 sites, found in the Gabbro paper test (2026-08-14)

`[profile.release]` in `Cargo.toml` sets **no** `overflow-checks` — in release an underflow is
thus **silent**, no crash. A paper test for Gabbro's range types then searched the tree
and found **18 uncovered subtractions**. **I re-checked two; both
exist.**

- [ ] **S3a — `kernel/src/system.rs:4092`: `total - src.len()` as the length of a `write_bytes` in
      `unsafe`.** The SAFETY comment *claims* `total >= src.len()` ("rounded up to pages"),
      **it is not enforced**. With `total < src.len()` a huge length arises and
      `write_bytes` runs beyond the frame — in release without warning.
      **That is the most serious of the 18.** An assurance that stands only in a comment is the
      class "a guard checks the EXISTENCE of a reason, never its TRUTH".
- [ ] **S3b — `kernel/src/system.rs:4405`: `(b[i] - b'0')` without digit check** over
      `env!("CAPROCK_MELDESTELLEN")`. **Milder than reported:** it stands in a `const` initialiser,
      and there rustc catches the underflow at compile time. What remains: the derivation silently
      assumes that `build.rs` delivers only digits — **an assumption without a check at a
      place that carries a ratchet.**
- [ ] **The remaining 16 are not re-checked individually.** Remarkable about the finding: **at three of them
      the same calculation exists elsewhere in the tree WITH protection** — so it is no design attitude,
      but one that is held unevenly.

**Why it stands here and not in the Gabbro folder:** these are sites in Caprock, independent
of whether Gabbro ever comes into being. A range type would have caught all 18 at compile time — that is
the by-product, not the point.

## Z28. Syscall ABIs as PLUGGABLE modules — decided 2026-08-13

**Class:** architecture · **State:** decided, not begun. Presupposes the payload of
[Z26/A3](#a3--the-kernel-primitive-is-built-2026-08-10-what-stands-what-is-open-and-the-threshold).

The kernel knows only `(guest, slot, occasion)` and **no semantics** — that is built. From it follows
an architecture that so far existed only implicitly: **the syscall ABI is a userland concept.**
Process A → Linux, B → WASI, C → native is a table lookup, not a kernel rework.

### The groups — the cut follows the SHARED STATE, not the syscall number

A decomposition "per syscall" would be nominal: 350 handlers in **one** PD share an address space,
so each reaches the memory of every other — the cap governs who is *called*, not who can
*read*. The load-bearing cut is the one along the shared data structure:

| Group | shares | Size |
|---|---|---|
| process/thread | TCB state, scheduling | small |
| time | clock, timers | tiny |
| randomness | entropy source | tiny |
| identity | uid/gid/credentials | tiny |
| synchronisation | `futex`, wait queues | small |
| memory | memory map | medium |
| fd/files | fd table, VFS | **large** |
| network | sockets | **large** |
| devices | device caps, DMA windows | medium |
| native services | Caprock caps directly | small |

**The distribution of sizes is the actual finding: six are tiny, two are large.**
Exactly that makes the proof stage below possible — *one verifies what is small and **isolates**
what is large.*

**The groups carry GRADED AUTHORITY, and that is their purpose.** The kernel thereby learns no
syscall semantics, but **authority classes**: a cap of the class "fs" may
touch guest memory, one of the class "network" must not touch a vspace. Five to ten named classes are
**auditable**; a free authority mask is not — the same decision as with
`IdentityReason` (closed enum instead of a selectable argument).

- [ ] **No catch-all group.** "Fits no group" needs a **named rejection**. An
      "Other" would become a dump and inherit the **union of all authorities** — then the
      classification is gone, and nobody notices.
- [ ] **Routing per RANGE, not only per guest.** Today: one handler per guest. A mixed
      runtime needs "0…350 to Linux, 400…750 to native". The step is small as long as the
      table is new.
- [ ] **No implicit passing on.** "Handler does not know N → falls through to Linux" would give a
      handler authority over a guest that **never chose it**. Not covered means
      **fault**; whoever wants a fallback level enters it expressly.
- [ ] **Cross-group syscalls cost several round trips.** `mmap` of a file touches memory
      *and* fs. The groups partition the **entry**, not the work. How expensive that is
      hangs on the number that does not exist in the whole tree (see below).

### DMA does not reach the kernel — and that already stands

The requirement is **structurally fulfilled**, not through diligence: `addr::Pa` and `addr::Iova` are
separate types, `DmaRegion::identity` was **deliberately deleted**, IOVA windows lie
**above `RAM_TOP`** with guard bands and without reuse — above `RAM_TOP` an
IOVA can never accidentally be a valid PA. It is enforced by the IOMMU, not by the driver.
A handler of the device group **cannot** therefore construct an IOVA that names kernel memory.

- [ ] **Fix as a property of the GROUP, not of its code:** the device group is the
      only one that passes on DMA-capable caps. Today that follows from the code; it belongs in the
      authority class.

### The proof stage: CONFINEMENT instead of correctness

**The entry ticket for a module is not "proved correct", but "cannot break out".**
That is cheaper by orders of magnitude and is already the pattern in the project: `caprock-part` and
`caprock-fat` are dependency-free and `forbid(unsafe_code)`, with exactly this reason —
*foreign disk bytes are interpreted nowhere with kernel privilege.* A wrongly read
FAT entry yields nonsense; it does not become code execution.

The property splits into four, three of them almost free:

| Part | how |
|---|---|
| **execute no foreign logic** | in safe Rust there is no way from bytes to a function pointer — that needs `transmute`. **`forbid(unsafe_code)` covers it mechanically** |
| **no authority outside the group** | the cap — i.e. the design anyway |
| **memory safety** | falls out with `forbid(unsafe_code)` |
| **termination** | **NOT free.** Safe Rust may loop for ever, and a hanging handler is a DoS for its guest |

- [ ] **Solve termination kernel-side, not by proof:** a **deadline** per redirected
      syscall, after which the guest faults with a named reason. That is the existing
      fail-closed rule with a clock — and it additionally covers the case that no proof
      covers: a handler that waits for something *else*.

> **The guard STANDS** (`tools/eingeschlossenheit.py`, 2026-08-13, in `tools/abnahme.sh`, 0.49 s):
> a module with `[package.metadata.caprock] einschluss = "streng"` carries `forbid(unsafe_code)`,
> has no build script and depends only on named crates; 9 modules declared, 15 speak checks
> in both directions, 0 candidates = return code 3. Details and the five findings that it
> uncovered in the process, in done.md — three of them in the **signed** unsafe audit of ADR 0014
> and therefore as an open item of their own further below. **This line stands here because the following
> items hang on it** — not as a success register.

- [ ] **What the guard does NOT know: which modules the primitive registers.** It checks what
      *declares itself*; a register table does not exist, so the question "is every registered
      module also declared?" is today structurally unanswerable. As soon as the table from the
      payload of [Z26/A3](#a3--the-kernel-primitive-is-built-2026-08-10-what-stands-what-is-open-and-the-threshold)
      exists, it belongs as a **second source** in the guard — otherwise membership is
      voluntary exactly as long as nobody notices.

- [ ] **The marshalling seam needs real attention — it is the one door.** There guest-controlled
      bytes meet handler code. `forbid(unsafe_code)` **and** checking the
      version header instead of interpretation. If it is not clean, the cleanliness of all 350
      modules is of no use, because all come through the same door.

**What remains and belongs named:** a confined module can still be **wrong**. That
is tolerable exactly when the decomposition holds — **one guest, one group** —, because then a
wrong handler is the problem of *its* guest and not of the system. This statement is the whole
gain, and it hangs on the cap boundary, not on the code.

### Full verification: where it carries, and where it would prove a wish form

- [ ] **For a NATIVE Caprock ABI the end-to-end proof makes sense** — the specification
      is ours. Start here first, with the smallest group (time or identity).
- [ ] **For Linux not.** "What Linux does" exists **nowhere formally**; one verifies against
      *one's own* specification, and whether that corresponds to Linux is unproved — exactly there
      live the compatibility errors. That is literally the trap "a proof that proves the wish form
      is worse than none" (`send_no_loss` did not hold at the real endpoint), and it coincides
      with the register finding on [Z16](#z16-translate-source-text-instead-of-running-binaries--assessed-2026-08-09): **WSL1 failed on behavioural fidelity**,
      not on memory errors. Verification buys memory safety and internal consistency —
      it does not buy behavioural fidelity. A verified Linux handler is therefore to be labelled **"proved
      against our model"**, not "proved".
- [ ] **Model-fidelity guard from the start**, not retroactively. Without it the model runs away from
      the code; this project has built three of them, because exactly that happened three times.
- [ ] **Keep the proof stage as a number:** how many modules with proof are registered and how many
      without — as a ratchet that may only fall.

### The number on which all of this hangs

**There is no cycle measurement of an IPC round trip in the whole tree.** It decides how fine the
decomposition may be (every group boundary in a chain costs a further round trip), whether a
handler thread per guest or a migrating thread is needed ([B](#b-thread-migration--remainder)) —
and whether the kernel under pressure gets ABI knowledge after all, which would end the whole pluggability.
The threshold has stood fixed since 2026-08-10: **redirected `getpid` ≤ 2000 cycles median**.

- [ ] **Measure as soon as the payload stands and the machine is quiet** — handler in an isolated PD
      **against** handler in `TrustedSas`; the difference *is* the price of isolation. In addition
      the bare IPC round trip as a base. Not under parallel load: this measurement rig has already
      once led the project astray with 3.2-fold overbooking (D13).

## The unsafe audit of ADR 0014 has three holes — found when building the Z28 guard (2026-08-13)

**Class:** checker form / chain of trust · **State:** open. None of the three is **effective** today;
all three are measured, not conjectured.

The certificate of a TrustedSAS PD carries `unsafe_status`, the kernel enforces at load
`== UNSAFE_ALL_PASS` (ADR 0014 §6, `crates/caprock-loader/src/cert.rs`). The bits are computed by
`tools/sign_trusted.py`. The statement is thus **signed** — and thereby a hole in its
computation is worse than one in an ordinary checker: it is cryptographically certified.

- [ ] **`has_forbid` accepts the attribute from a COMMENT.** A free `re.search` over the
      raw file content; the mere mention `` `#![forbid(unsafe_code)]` `` in a
      doc comment suffices. In `crates/caprock-loader/src/lib.rs` line 3 there is such a
      mention (there in addition to the real attribute, i.e. without effect). `UNSAFE_PROGRAM_FORBID`
      can thereby be **earned with prose**. The remedy stands beside it:
      `tools/eingeschlossenheit.py` removes comments and anchors at line start.
- [ ] **`unsafe{` without a space is not counted.** `RX_UNSAFE` demands `\s+`; `unsafe{ … }`
      is valid Rust. **Measured: 0 occurrences in the whole tree** — the hole is unused, not
      absent. The one-liner is `\bunsafe\b\s*`; the tightening can only find more, never
      less.
- [ ] **The only crate of the allowlist has a build script.** `programs/libcaprock/build.rs` runs
      at build time on the **host** with full authority; `forbid(unsafe_code)` does not touch it,
      and the audit sees only `src/`. Harmless today (RUSTFLAGS check), but it sits in the
      chain of trust of **every** TrustedSAS program. Either take it into the audit or
      expressly keep it as debt — silently outside is the third possibility that
      must not exist.
- [ ] **Two parsers for the same condition would be the next crack.** The Z28 guard holds its
      set today against `ALLOWLIST` in `sign_trusted.py` (with an anchor test). Right would be **one**
      count with two consumers — the same pattern as `tools/mangel-zaehlen.py`, which
      `kernel/build.rs` and `tools/mangel-stellen.sh` use together.

## Two foreign-byte parsers are confined in the MATTER, not in the TYPE (2026-08-13)

**Class:** hardening · **State:** open, measured with `tools/eingeschlossenheit.py --liste`.

`caprock-dtb` (device tree parser) and `caprock-abi` have **0 `unsafe` and no
dependencies**, but carry no `#![forbid(unsafe_code)]`. For `caprock-dtb` that is exactly the
class of `caprock-part`/`caprock-fat`: **foreign bytes** that are read in the boot-up. "No
`unsafe` today" is a snapshot; `forbid` is a condition.

- [ ] Give both `#![forbid(unsafe_code)]` and declare them — or decline in
      `AUSDRUECKLICH_NICHT`, with a reason. Two lines per crate; the guard draws the
      consequence by itself (it then also checks transitively).

## Z. Target architecture (as of 2026-07-29) — against which everything else is to be measured

> **Order and reasons:** [docs/plan-betriebsbereit.md](docs/plan-betriebsbereit.md).
> Here stands *what* is open, there *in which order* and *why*.
>
> **Split into two independent strands for parallel work:**
> [todo-A-ausfuehren.md](todo-A-ausfuehren.md) — the kernel executes foreign code and exchanges it
> (manifest, root task, caps, hot reload, drivers). **Critical path.**
> [todo-B-verlaesslichkeit.md](todo-B-verlaesslichkeit.md) — the assurances carry and are
> measurable (determinism, ARM branch, IOMMU, colouring/SMT, time/NUMA, attestation).
> File ownership and the four real couplings stand at the end of strand A.

The microkernel is **the base system of a cloud server**, not a hypervisor beneath it and
no VM inside it. There is no guest layer: isolation, time and resource management are done by the
kernel itself. From this follow three requirements that are not pure diligence, but co-determine
the design — and on which the sections A–G below hang.

**The load form (Simon, 2026-08-01): Vercel-like services.** Short-lived function calls, high
turnover, **many tenants per machine**, foreign code. That is no marginal note, but fixes
three quantities that would otherwise look freely selectable:

* **Spawn/teardown is the hot path**, not the start path. Exactly there sat the D0 races.
* **The number of simultaneously separated tenants is 4 today.** `PARTITIONS = 4`, and `claim_stripe`
  fails cleanly from the fifth (B-4.2) — right so, but four cache-separated tenants per
  machine are, for this load form, too few by orders of magnitude. And raising it is not free:
  `region_bytes()` is `min(colours, 64) / PARTITIONS` pages, at `PARTITIONS = 64` thus **one
  page** per contiguous region. This tension is the actual content of B-4.1.
* **The machine must be bare metal.** Newly proved on 2026-08-01 (B-4.5): if Caprock itself runs as a
  guest, the host rewrites the colour bits, and the cache separation is ineffective — measured, not
  conjectured. Whoever sells "isolation without VMs" and uses rented VMs for it sells nothing.

**Z1. No VM overhead, isolation entirely by the kernel.** The isolated path (own VSpace
per PD, coloured) must become the **normal case**, not the exception. Today `spawn_isolated` is the
regular way and uncoloured; `spawn_isolated_colored` is the special way (see [A1](#a1-cachetiming-side-channels-between-pds)).

**Fixed rule (Simon, 2026-07-29): TrustedSAS is exclusively for our own code. No
customer code ever runs in a TrustedSAS PD.** That is no precaution, but follows
from the model: TrustedSAS PDs share an address space, and their isolation rests on
*intralingual* safety — safe Rust cannot forge a pointer to foreign memory. This
precondition can be checked only on our own source code (the certificate gate ADR 0014 does exactly
that: `unsafe_status == ALL_PASS`), never on a foreign binary.

Three consequences that bind the design:

* **The isolated path is the product path.** Its performance and its scalability *are* the
  product — not those of the SAS path. Optimisation work belongs there.
* **The certificate gate is an operator tool, no customer interface.** Customers need
  no certificate and get none; a way by which a customer could obtain a TrustedSAS certificate
  would be a design error. Conversely that means: [Z7](#z7-attestation-and-measured-boot)
  concerns the **machine**, not the customer binary — the customer wants to know *what* they run on,
  not prove *what* they bring along.
* **Two classes, two assurances.** Own services (drivers, network stack, control plane) may be
  TrustedSAS and pay no address-space switch for it. Everything foreign is isolated, coloured
  and accounted that way. That belongs in `docs/invariants.md`, so that the boundary is not later blurred out of
  convenience.

**Z2. Accounting finer than the clock tick — "only pay for what you need".** 100 Hz means 10 ms
granularity; that is at once the lower bound of accounting *and* a jitter source that HPC
switches off precisely. The goal is an accounting that does **not** hang on the tick rate: consumption is
stamped via TSC/CNTVCT on switching in and out (cycles, not ticks), and the tick itself
is switched off for compute cores instead of accelerated. A faster tick would be the wrong
answer — it raises the resolution *and* the overhead; a cycle stamp raises only the resolution.
See [Z5](#z5-tickless-for-compute-cores) and [D](#d-verification) (per-TCB `consumed_cycles`).

**Z3. Freeze threads, move them, let them continue running (across servers).** That is the
property that VMs deliver today (live migration) and that a VM-free design must replace.
It is the **most demanding** requirement in the whole document, because it cuts across almost everything:
a thread is not only its register set, but its memory, its caps, its
IPC relationships and its position in the kernel tables. Decomposition in [Z4](#z4-checkpointrestore-of-a-thread).

### Z11. Boot image = kernel + **one** manifest file; everything else outside and exchangeable
**Class:** structure/operation · **Effort:** large, affects loader, cap system and IPC

**Fixed rule (Simon, 2026-07-29):** In the boot image lie exactly two things — the kernel and
a file that fixes *what* is loaded. Everything else (drivers, network stack, services,
control plane) lies outside the image and is **exchangeable at run time without IPC
relationships being lost**.

That is the sharper version of the existing project boundary ("only microkernel, caps, scheduler, IPC
and Microkit runtime in the image") and draws consequences that go beyond it.

- [ ] **Z11a. Name and solve the chicken-and-egg problem.** *(Bootloader way proved 2026-08-01:
      `tools/mkgrubiso.sh` builds a GRUB ISO from kernel + boot archive; a real bootloader
      demonstrably delivers the same as QEMU's `-kernel`/`-initrd` — memory plan, one module,
      `mbmod : ALL PASS`, `archive : 2 Modul(e) -> ALL PASS`, root task runs,
      `SELFTEST COMPLETE`. The only difference: the load address of the module, as expected.
      The worry about `Flags = 0` in the Multiboot header was unfounded — GRUB delivers
      memory plan and modules even unasked. **The second half remains open:** reloading
      at run time. The disk driver for it is there and has run since A-5.1 (2026-08-01)
      as a **loaded userland program** — it serves read requests via its channel, checked
      in content. What is still missing is everything above it: a partition table and a file system.
      A sector is not yet an archive.)*
      A disk driver cannot be loaded from
      the disk. The only honest way out: **the bootloader delivers the start modules**
      (Multiboot modules with GRUB) — they thereby lie outside the kernel image, but still
      arrive at boot. The manifest names them; the kernel checks that exactly what stands
      there arrived. Reloading via network/disk exists only when the associated drivers run.
      Without this separation ("start set from the bootloader" vs. "reloading at run time") the
      manifest becomes a wish list that nobody can redeem.
- [~] **Z11c. The policy stands in the manifest and is APPLIED** (2026-08-07). Until then the
      kernel read the fields and **printed** them; only `POLICY_ROOT_TASK` was observed. A
      policy field that is only printed is worse than none — it looks configured.

      | Field | State |
      |---|---|
      | `POLICY_EXCLUSIVE_STRIPE` | **observed** (A1, `pdcolor : ALL PASS`) |
      | `priority`, `core_affinity` | **observed** (`ladepol : ALL PASS` — prio 2 demanded, prio 2 received) |
      | `POLICY_ROOT_TASK`, `POLICY_NO_HOTRELOAD`, `iface_version` | observed (A-2.1 / A-4.5 / A-4.4) |
      | `numa_node != 0` | **rejected** — the allocator has no nodes (Z8) |
      | `POLICY_PINNED` | **rejected** — "load balancing is off by default" is no assurance |
      | `budget_us != 0` | **rejected**, see the open item below |

      **Open: `budget_us` cannot be observed as a format.** An MCS reservation needs budget
      **and** period; the manifest has one number. To make a reservation from one number would mean
      inventing the period — it would then stand in no document. In addition the resolution: `set_budget`
      computes in ticks (100 Hz → 10,000 µs), everything below would be 0 or rounded up. That is a
      **format question**: either a `period_us` field (the reserved bytes are gone, so a
      new `entry_len` and thereby a format version), or `budget_us` falls.

      **Open: `priority = 0` cannot be told from "nothing said".** 0 is in the scheduler
      the lowest valid priority; today an entry with 0 gets the default. Whoever really wants 0
      cannot say so. The same format question as above.

      **DONE, and it is the precondition for both: the rejection on an unknown format is
      LOUD.** (2026-08-10) The rule, stated before anybody touches the format version:

      > A loader that meets a `format_version` or `entry_len` that it does not know
      > **rejects** — and the rejection says that it is a **version difference**, not a
      > form error. It **never** reads out the fields known to it and skips the rest.

      Both halves are necessary, and the first is the sharper: **what is signed is `[0..msg_len)`,
      i.e. the whole message.** A v2 manifest from which a v1 loader reads 96 of 104 bytes per entry
      would be **authentic and not understood at the same time** — a valid signature proves
      authenticity, **not understanding**, and the byte offset would shift precisely `initial_caps`,
      `policy_flags` and `priority`: an **authority allocation**, silently slipped.

      The parser had rejected before — but **silently**: six structurally different
      reasons ended in one `BadManifest`, and seven host tests fixed this
      indistinguishability. Two of them mean "newer than this kernel", four "broken
      bytes", and they lead to **opposite** actions. In the kernel it ended as
      `im Archiv liegen N B, die nicht parsen` — three lines beneath a comment that already draws exactly this lesson for the
      **kernel hash** ("the most frequent real cause is *new
      kernel, old manifest*, which is why it says here WHAT was bound to — otherwise one guesses").
      Drawn for the hash, not for the format version. And one file further on `Archive::parse` has long
      distinguished **five** named reasons — precisely the more security-critical
      parser did not.

      New: `LoaderError::UnsupportedManifestFormat { format_version, entry_len }` — **with the
      numbers**, because a rejection without them is the same guessing exercise as before. Kernel report line
      with both values *and* what this kernel can do. Host tests: the four form cases remain
      `BadManifest`, two new cases yield the new variant with the right numbers, in addition a
      **speak check of the distinction itself** (`assert_ne!`) — without it both tests would be green even
      if the variants had the same value. System negative case 3b in the load suite:
      `entry_len=104` → named rejection, and the number 104 appears only if the kernel really took
      this branch.

      **The addendum that belongs to the honest version:** the rejection falls **before** the
      signature check — it must, one cannot verify what one cannot parse. The
      diagnosis is thereby **unauthenticated** and can be provoked by a flipped byte. What the
      signed header carries is the **other** direction: an *accepted* manifest cannot have a
      planted format version. Both halves stand in the code; naming only the pleasant one
      would make of it an assurance that does not hold.

      Thereby the way to format version 2 (`period_us`, sayability of `priority`) is free: a
      v2 manifest on a v1 kernel is from now on **distinguishable from corruption**. Before,
      the format leap would have been a field deployment with an error message pointing
      in the wrong direction.

      **A finding on the side that is bigger than the entry:** the priorities had always stood
      in the test manifest (3/1/2/2/2) and were never honoured — they were placeholders. Observed,
      the same allocation **tears** the load suite: a *polling* driver (B-3.2, no IRQ) at
      a higher priority than its client starves the client. Allocated rightly
      and nevertheless unusable. A field that is never honoured accumulates values that nobody has checked
      — and the day it is honoured is the day they are all wrong.
      (Exactly for that reason a polling driver needs a **budget**, see above.)

- [ ] **Z11d. Hot reload without IPC loss — what is really necessary for it.** On ARM the
      case already exists (phase 7: a server PD is replaced via the *same* endpoint cap). For an
      operational claim that is however not enough; three things are missing:
      1. **Endpoint identity independent of the thread.** Clients hold caps on the *endpoint*,
         not on the server thread — that already carries today. The exchange is then a
         rebinding of who receives on the endpoint. That must be **atomic**: between "old
         server gone" and "new server ready to receive" there must be no state in which a
         `CALL` fails with `NoEndpoint`.
      2. **Pending transactions.** A client blocked in `CALL` holds a reply cap
         on the old server. Either the new server inherits the open replies, or the
         exchange takes place only at a *resting* point (no open transaction). The
         second variant is honest and probably right for stage 1 — but it needs
         a concept of "resting" that does not exist today. Related to [Z4a](#z4-checkpointrestore-of-a-thread):
         the same stopping-point concept carries both.
      3. **State hand-over.** The new version needs the state of the old one. Either a
         memory region that survives the exchange (then its format is an ABI and must be
         versioned), or an explicit hand-over protocol. Without a determination hot reload
         becomes a restart with data loss and is merely called something else.
### Z4. Checkpoint/restore of a thread
**Class:** new subsystem · **Effort:** large, multi-stage

Order is here no matter of taste — every stage is the precondition of the next.

- [~] **Z4a: the stopping point stands** (2026-08-02, `system::freeze_thread`, check line `freeze`).
      Details in done.md.
      "Nameable" means two checkable things: **not running on a core** (asked via `current_id` per
      core, not believed) and **no open IPC relationship**. The three outcomes are
      distinguishable, and the difference carries: `StillRunning` is transient, `Busy` is a
      **rejection** — whoever waits for it waits for ever. What is checked is the **effect**: the round counter
      must move beforehand, then stand, then run again; two mutations turn the suite
      red.

      **The syscall remains open.** `freeze_thread` is a kernel function, no `SYS_FREEZE`,
      and it does **not wait** — it takes the scheduler lock of each core, and waiting in it for
      another core is the construction manual for a deadlock. The call is idempotent;
      waiting belongs to the caller. A blocking syscall above it is an ABI question and
      a stage of its own.
- [~] **Z4b: the refusal rule stands, the serialiser does not** (2026-08-02,
      `crates/caprock-cap/src/checkpoint.rs`, 7 host tests). The register set was the
      easy part anyway; built is the hard one: **what cannot designate the same thing on the
      target machine does not travel along — it is refused, not replaced.** Refused are `Mmio`,
      `Irq`, `Dma`, `Reply`, `Loader` and every relationship cap whose partner is not in the scope;
      the reasons are **individual** and no collective `false`, because only some can be remedied by a larger
      scope. Fail-closed: a new object type does not travel along, instead of
      slipping through.

      Three decisions are in the tests: memory travels as **content** (two regions
      of equal length at different physical addresses are externally equal), relationships via the
      **place in the checkpoint** instead of via an ID, and a budget carries a **precondition**
      (`SameTickSemantics`, Z4f) instead of concealing it.

      ~~**Open:** a serialiser and a restorer.~~ Both have stood since 2026-08-03,
      see stage 2 directly below.
- [~] **Z4 stage 2: a thread across the BOOT BOUNDARY** (2026-08-03, check line `bootckpt`).
      Details in done.md. The same kernel
      saves and restores and **decides itself which** — it reads the sector
      (`CKPT_SECTOR = 32710`, outside both partitions). Transport is the existing
      block service; the core is client, `programs/hardware/virtio-blk` remained unchanged. The
      format lies in `crates/caprock-cap/src/checkpoint.rs` (fixed widths, LE, CRC-32,
      host-tested), and `Image::build` calls `classify_all`: **a non-transferable cap
      prevents the checkpoint before the write.**

      **The finding that rebuilt the design belongs in the memo list:** the progress counter
      is under KVM **reproducible** (measured 133…155 at the same point of the boot-up). A
      mutation that reads and reports the found value without setting it hit the stored
      value exactly — and the suite stayed green. Only a **growing chain** (every run works on
      +100 rounds and saves with epoch + 1) separates "inherited" from "counted itself"; the
      positive control for it (`Zaehler-vorher != Fortschritt`) stands in the suite.

      **What remains open here:** no trap frame and no registers — that would be worthless without
      [Z4c](#z4-checkpointrestore-of-a-thread) or worse, because `RSP` points into a stack
      whose content is fresh memory after the restart. And the checkpoint is
      **not authenticated** (Z4e): the checksum says "structurally intact", not "from whom".
- [ ] **Z4c. Take memory along.** The private region of the PD is the state. At 64 KiB
      ([A1](#a1-cachetiming-side-channels-between-pds)) that is cheap, at 2 MiB less so.
      Open: dirty tracking, so that not everything has to be copied, and the question whether the
      **colour** must be preserved on the target machine (no — colour is machine-local; the
      target machine colours anew; that is an argument for never lifting colour into the ABI).

      **Since stage 2 this is the BLOCKING stage, not a concurrent one.** The checkpoint
      carries a memory cap as `Region { len }` — as **length**, not as content; over there it
      is null. And a trap frame can sensibly be taken along only when the stack
      comes along: `RSP` without stack content is a pointer into the void, and the error occurs later and
      elsewhere.
- [~] **Z4d. IPC relationships — stage 1 stands** (2026-08-25, check line `ckptcut`). Details in
      done.md. Chosen is
      the first variant ("migration only without open transactions"), and it is now a
      **structure** instead of a call discipline: `Image::build` takes the subject and the
      **observed** IPC edges and rejects at the first one that crosses the cut.

      **The rule is an equivalence:** *the channel travels ⇔ the thread that holds a role there
      travels.* Both directions are rejections, and they are **different** rejections — the one
      is remedied by taking the channel into the scope, the other by taking the partner
      along or waiting out its transaction.

      **The finding that is bigger than the entry:** the second direction had been written down by
      nobody. `Scope::endpoints` carried the doc "endpoints whose **both sides** are part of the
      checkpoint" — a list that the caller writes down, and until 2026-08-25 nobody
      held it against the IPC state of the machine. A named endpoint was
      **believed**; a client blocked at it that stays behind occurred nowhere. The same
      form as `ep_inv` ("was held not by the type, but by call discipline") and as "a guard
      checks the EXISTENCE of a reason, never its TRUTH". `Scope::EMPTY` concealed the hole
      — with an empty scope every relationship cap is rejected, the unchecked piece was thus unreachable from the
      only call site. An unreachable hole is one.

      **Stage 2 remains open:** endpoint proxies over the network, so that a relationship can
      *survive* the machine boundary instead of forbidding it. That is a project of its own and
      hangs on [Z4e](#z4-checkpointrestore-of-a-thread) (transport) and [Z10](#z10-io-at-all)
      (network).

      **And a weakness that belongs to the line:** `ckptcut` gates via the `check` of the two
      suites, **not** via `all_done()` — `ckptcut::urteil()` is dead, exactly like
      `pdfreeze::urteil()` and `dbgprobe::urteil()`. Stands as an item of its own under
      [Z23](#z23-process-freeze--the-group-cut-stands-2026-08-21).
- [ ] **Z4e. Transport + trust.** A checkpoint is the complete state of a tenant.
      It goes encrypted and authenticated over the network, or not at all. Hangs on
      [Z7](#z7-attestation-and-measured-boot): the target machine must be able to prove that it
      runs the same kernel version with the same assurances — otherwise the state migrates into a
      weaker environment, and the tenant does not notice.
- [~] **Z4f. Name identical servers as a precondition.** "Same server" must be
      checked, not assumed: same architecture, same kernel version, same
      cache/NUMA class. A comparison that checks only the architecture lets a thread
      migrate from a machine with `invtsc` to one without — and the accounting from [Z2](#z-target-architecture-as-of-2026-07-29--against-which-everything-else-is-to-be-measured)
      becomes silently wrong.

      **A piece of it stands (2026-08-03):** the checkpoint carries `kernel_code_hash` and is
      rejected if it belongs to a different kernel image (read code 7, negative case in
      `test-qemu-x86-load.sh`). That is the **kernel version**, and only it.

      **Not checked — and that is exactly the gap that this item means:** the MACHINE. Two
      computers with the same image, but different cache/NUMA class or one without `invtsc`,
      are today not distinguishable. The precondition `SameTickSemantics` stands **in** the checkpoint
      (`precondition_bits`), but nobody evaluates it — to carry it and not check it is
      the half way, and the half way looks from outside like the whole.

#### Z12. Describe and generate wire formats declaratively (proposal Simon, 2026-08-07)
**Class:** tool/userspace · **Effort:** small for the first cut, grows with the scope

**The idea:** foreign protocols (Ethernet, later IPv4/UDP, EDID/display) stand as a
description file in the repo and are translated to Rust. A new or old format then costs
one file instead of one parser.

**That fits the kernel boundary, and exactly.** Generated parsers are pure functions over
byte slices — no caps, no syscalls, no allocation. They belong where `caprock-part`
(502 lines, 0 dependencies, `forbid(unsafe_code)`) and `caprock-fat` (703 lines, likewise)
already stand: **userspace**. The generator is a **build-time tool** on the host; in the kernel
nothing changes.

**Where the yield really lies.** Not in convenience, but in the two errors that
hand-written parsers actually make: **endianness** and **range checking**. A generator
does both uniformly, and with `forbid(unsafe_code)` plus always length-checked accesses
that is a security gain, not a comfort gain.

**The scope decides about the benefit — and it is unevenly distributed:**

| Format | Share that is LAYOUT | Suitability |
|---|---|---|
| Ethernet II / 802.3 / VLAN, ARP | almost everything | **very good** |
| IPv4/UDP headers | large (options are the rest) | good |
| EDID (128-byte blocks) | almost everything | good |
| TCP | the header is a fraction | bad — the state machine is the work |
| display/graphics in general | little (registers, timings, framebuffer) | bad — no wire format |

**"Change only the protocol file, nothing in the code" holds for READING and WRITING, not for
BEHAVIOUR.** A table generates the header; it generates no ARP resolution, no DHCP, no
retransmission, no mode setting. With Ethernet/ARP the ratio is favourable (the frame *is*
almost the whole work), with TCP it is not. That should determine the scope, not
enthusiasm.

**The trap, and it is the known one:** if sender and parser come from the **same** description,
a wrong description makes both **wrong together** — and every check line stays green.
Literally the form of D8/D9/D11 ("the suite never triggered the error") and of `send_no_loss`
("the proof was right, the code was wrong"). The remedy exists here already:
`tools/checkfat.py` reads the image in a **different language**, with the pattern written out
again. For frames that means: a **golden hexdump from a real capture**,
checked by hand and **not** generated from the description.

**On the format: YAML is the most expensive of the possible choices.** `serde_yaml` has been
archived since March 2024, the successors fragment (`serde_yaml2`, `serde_yml`, `yaml-serde`) without an
obvious winner. In addition a layout description needs more than a flat table
(variable lengths, TLVs, conditional fields) — and exactly there declarative formats either grow into
a programming language or fall short. Two honest ways:
* **Kaitai Struct** — a mature DSL with a compiler, already generates Rust, and there are ready-made
  `.ksy` files for Ethernet/IPv4/EDID. Price: a Java compiler in the build path and generated code
  whose form (allocation, `no_std`) one does not determine. **Worth a try before it is
  decided** — inherited descriptions are a real saving.
* **A tiny description of our own** (TOML or line-based), parser dependency-free in
  `tools/`. Fits the house style and leaves the form of the product in our own hands.

**Proposal for the first cut — on existing code, not on a greenfield:**
1. Describe **Ethernet II + ARP**. Both stand today hand-written in
   `crates/caprock-virtio/src/net.rs` (`ETHERTYPE_ARP`, `ARP_REQUEST/REPLY`, 42-byte frame built by
   hand) — so there is a before and an after to compare.
2. Product as a **dependency-free, `forbid(unsafe_code)`** crate, **checked into the repo**:
   the build runs without the generator, and the diff is readable.
3. **Guard on the pattern of `tools/identitaet.sh`:** the checked-in product must be
   byte-identical to what the generator produces now — with a self-test that proves that it can
   fail.
4. **Independent witness:** golden hexdump per frame. In addition the existing
   ARP round trip (`virtio-net`, A-5.2) carries the behaviour statement — it proves sending *and*
   receiving in content.
5. Only afterwards decide about IPv4/UDP and EDID.

**Acceptance condition:** the hand-written frame construction in `net.rs` disappears, and the
ARP check line stays green — same behaviour, one source.

**What it does NOT solve:** Z10 (no network stack). The generator is the smaller half; whoever takes it for
the stack confuses the header with the protocol.

**A side finding during the investigation, an item of its own:** the same problem already exists, but with
**our own** service protocols — see Z13.

### Z14. Foreign software without a guest layer — assessed 2026-08-09
**Class:** product gap · **Effort:** staged, the first stage is small · **Boundary condition
(Simon, 2026-08-09): isolation and the very small TCB are preserved.**

**The number that sets the frame:** the delivered TCB is `.text` **212 KiB** + `.rodata`
24 KiB (release without `selftest`; with test code 360/52 KiB). Everything assessed here must not
raise this number.

- [ ] **The actual state, measured.** The ABI has **17 syscalls**, all cap-based. For foreign
      software four things are missing, and three of them are no trifle:

      | Missing | Finding |
      |---|---|
      | `brk`/`mmap` | **no way to get memory at run time.** `SYS_MAP` maps what the PD **already holds**; its memory is fixed with the endowment at load |
      | threads in a PD | no creator in the ABI (0 hits for `SPAWN`/`CLONE`) — a PD has **one** thread |
      | clock | no time ABI (0 hits for `CLOCK`/`GETTIME`) |
      | dynamic linking | `PT_INTERP` is handled nowhere — only static binaries |

      **What already exists against that and what is the key:** cap transfer via IPC (the
      `xfer` test delegates a cap by REPLY). A PD can give another authority
      **without the kernel learning anything new**.

- [ ] **Arbitrary Linux programs: possible — and my first assessment was wrong** (corrected
      2026-08-09).

      I had written that this was **excluded** by the TCB boundary condition, because the
      kernel would have to "know 300+ Linux syscalls". That is the wrong design, and it is not
      the only one. The right one has stood in seL4 for twenty years: **the kernel knows not
      a single Linux syscall — it redirects them.**

      **The mechanism.** A PD carries a bit "foreign personality" and a handler cap.
      If it is set, *every* syscall from this PD is not dispatched, but sent as an IPC message
      to the handler — register content as message words. The handler is an
      ordinary PD and emulates there what Linux promises. Kernel cost: one bit per PD, one
      branch in the syscall entry, and the existing IPC send path. That is no estimate of
      300 syscalls, but of **one branch**.

      **What beyond that is missing in the kernel — four bounded operations:**

      | Missing | for what | why bounded |
      |---|---|---|
      | syscall/fault redirection to a PD | every Linux syscall, every page fault (COW!) | one bit + one branch; today a fault terminates the thread in the kernel |
      | put a thread into an **existing** PD | `clone`, pthreads | today a PD has **one** thread (0 hits for `SPAWN`/`CLONE` in the ABI) |
      | map/unmap into a **foreign** PD | `mmap`, `mprotect`, `ld.so`, COW | `SYS_MAP` today maps only into the **own** VSpace |
      | read/write the thread context of a foreign PD | signals, `ptrace`, COW resumption | cap-gated like `PDCTL` |

      Together roughly 500–800 lines, i.e. **5–8 % TCB growth** (212 KiB → ~225 KiB). Not
      nothing, but far from "excluded". Each of these four is in addition useful on its own
      — fault redirection, e.g., would be the first step towards a pager in userspace.

- [ ] **And nevertheless it is the wrong bet. The reasoning is empirical, not aesthetic.**

      * **"Arbitrary" is the problem word.** Ninety per cent of programs need ~50 syscalls;
        the last ten need the other 250 **plus** `/proc`, `/sys`, netlink, epoll,
        io_uring, namespaces, cgroups — interfaces that are no syscalls and whose
        behaviour is specified nowhere except in the Linux source.
      * **gVisor** implements about 260 syscalls in ~100,000 lines of Go and still has gaps;
        it is a Google project with a decade of work.
      * **WSL1 was exactly this design** — a Linux personality in the NT kernel. Microsoft
        **abandoned it and replaced it with a VM** (WSL2), because ABI fidelity and
        file system performance could not be caught up. That is the strongest datum available, and it
        comes from somebody with more means than this project.
      * **The TCB advantage is annihilated for the Linux tenant itself.** For the system and for
        other tenants the TCB stays small — for the Linux program it consists of kernel **plus**
        personality server. Whoever carries "small TCB" as a product promise must deliver this sentence
        along, otherwise it is dishonest.
      * **Performance:** every syscall becomes an IPC round trip. gVisor pays 2–10× for that with
        syscall-heavy loads. The IPC fastpath of this kernel helps, but does not remove it.

      **Assessment:** technically possible with bounded kernel growth, economically a
      multi-year strand with a known bad outcome. WASM delivers the largest part of the
      compatibility benefit at a fraction of the cost and **supports** the product thesis instead of
      undermining it.

      **If it is done nevertheless, then in this order:** the four kernel operations
      first and accepted individually (each is useful on its own), then a personality for
      **statically linked, single-threaded** programs, then `clone`, then `fork`. And the first
      measurement is not "does busybox run", but: **how many distinct syscalls does the
      target application really call?** `strace -c -f` on the target program is an hour's work and
      decides the whole strand.

      What is **not** excluded and much cheaper: the same source text, **relinked**.

- [ ] **Stage 1 (small, and precondition for everything further): a memory server in userspace.**
      A PD holds a large memory cap and hands out derived caps on request via IPC REPLY;
      the client maps them with `SYS_MAP`. That is `brk`/`mmap` — **without a single new
      kernel line**, because cap derivation (`CCOPY`) and cap transfer both already stand.

      The kernel stays untouched, the policy ("who gets how much") moves to where it
      belongs: into a PD and into the manifest. That is the same step as A-5.1 for the driver and
      A-6.3 for the file system.

      **Acceptance:** a PD that gets 4 KiB at load requests 1 MiB at run time, writes
      it full and gives it back — and a second PD is **not** shown the same region
      (positive control over the same server, only one request moves).

- [ ] **Stage 2, three ways — and they do not exclude one another.**

      | Way | TCB effect | what runs with it | Price |
      |---|---|---|---|
      | **WASM in a PD** | **zero** (the engine is userland) | everything that compiles to WASM/WASI — today the bulk of what "Vercel-like services" stands for | no existing native binaries; a `no_std` engine must come in |
      | **musl relinked against `libcaprock`** | zero | almost everything that builds from source | libc port: file descriptors, paths, threads, signals — each an own service |
      | **own compat library** | zero | what one builds against it oneself | most honest, but smallest reach |

      **Assessment:** WASM fits the product thesis and the boundary condition best — the sandbox
      lies **in** the PD, not in the kernel, and the WASI set is small and closed (unlike
      the Linux set, which by definition is not). The memory need is known at
      start (WASM linear memory), so even today's model carries without stage 1 —
      though without `memory.grow`.

      The musl way is the one with the largest reach and the largest price; it becomes
      sensible only when stage 1 stands **and** a file system service with paths exists (today:
      FAT16 over a fixed file, A-6.3).

### Z27. Binary compatibility is from now on a LINE — the reversal of Z16, decided 2026-08-10
**Class:** direction decision · **Occasion:** [Z26](#z26-gpu--the-plan-2026-08-09-measured-on-this-machine)/addendum 1, redeemed when building A3.

**This decision REVERSES A RECORDED ONE, and that is why it stands here and not in a
cap design.** Without this entry two strands with contradicting premises would stand
in the register, and in three months nobody would know which applies.

**What Z16 said** (2026-08-09, verbatim): *"What this goal STRIKES, and that is the actual
yield: binary compatibility falls away. Thereby all four kernel operations from Z14 fall away —
**no syscall redirection**, no `ld.so` for foreign binaries, no ABI fidelity towards Linux,
no personality server."*

**What applies from today:** the syscall redirection is **built** (Z26/A3, see below). Thereby Z16 is neither
false nor superseded — it is **one of two lines**:

| | Z16 — translate | Z27 — drive |
|---|---|---|
| Input | source text | unchanged Linux ELFs |
| Requirement | POSIX **semantics** | Linux **behaviour** |
| Kernel cost | one kernel operation (threads in a PD) | the redirection primitive + three authorities (see A3) |
| Precedent that carries | Redox OS (`relibc`) | Fuchsia/Starnix |
| Precedent that failed | — | WSL1, gVisor |

**The reason for the reversal** — and it is not "nicer", but **unavoidable**: `libcuda`,
`libnvidia-glcore` and the remaining NVIDIA userland libraries do **not exist as source text**.
Z16 structurally cannot reach them, no matter how good it becomes. The same applies to every
customer load that nobody recompiles, and to Go (own runtime, bypasses libc on principle).
Whoever wants "PaaS with GPU compute" has no choice here, but a bill.

**And now the demand that the old analysis makes on the new one — it is the actual
content of this entry:**

> **WSL1 did not fail on the redirection mechanics, but on BEHAVIOURAL FIDELITY. A cap
> makes that not one gram easier.**

The primitive from A3 solves the problem "how does the syscall get to the personality". It solves **nothing**
of what WSL1 died of:

- [ ] **The surface is not the number of syscalls, but the number of observable
      behaviours.** Z16 measured 48 distinct syscalls over five programs. For
      Z27 this number is **the wrong quantity**: `ioctl` is ONE syscall and an unbounded
      surface; `/proc` is no syscall at all; `futex` has dozens of submodes with
      timing behaviour. Before anybody estimates effort, **a measure of fidelity** must be
      fixed. "48 syscalls, of which we have 30" would be exactly the sort of green that this
      project does not let pass with a checker.

- [ ] **Acceptance is a foreign test suite, not the own checker.** The same rule as in
      Z16 ("translated is not working"), only sharper: here there is not even a
      compiler that contradicts. A candidate that **measures instead of claiming** is the
      **LTP** (Linux Test Project) — number of passed cases per subset, into the log,
      with date and sample size.

- [ ] **Where the two lines contradict each other, the USE CASE applies, not the
      order in the register.** Z16 remains the cheaper way for everything that exists as source text
      (and that is the larger part of a server OS). Z27 is the only way for
      blobs. A program that runs via Z27 although its source text exists pays the
      redirection costs for nothing.

- [ ] **This reversal moves Z14 onto the critical path** (it stood there as "assessed,
      deferred"). The most expensive part of the GPU undertaking is thereby **not the driver**.

### Z26. GPU — the plan, 2026-08-09 (measured on THIS machine)
**Class:** assessment · **not begun** · Occasion: GPU code is what one writes least oneself,
and NVIDIA userland is proprietary.

## The two measurements that decide everything

| measured | Value |
|---|---|
| `modinfo -F license nvidia` | **`Dual MIT/GPL`** |
| `/lib/firmware/nvidia/610.43.03/gsp_ga10x.bin` | **84 MB** |
| GPU | `10de:2d18` GB206M, RTX 5070 Max-Q (Blackwell) |

**Both turn the expectation.**

**First: the NVIDIA KERNEL DRIVER is open and MIT.** Since 2022 these are the
`open-gpu-kernel-modules`; on this machine exactly that variant runs. MIT is
**compatible** with AGPLv3 — the driver would **not** have to go through the GPLv2 zone. That is the most
favourable licence situation of all drivers considered so far, more favourable than NVMe (21/21 v2-only).

**Second, and that is the actual lever: 84 MB of firmware.** Since Turing the
driver logic runs on the **GSP** (a RISC-V core *on the GPU*). The kernel module is thereby
largely **firmware loader + RPC transport**, not a monolith of hardware logic. The
order of magnitude named in Z20 for i915 (1.9 MB module) does not apply to NVIDIA Blackwell
so.

## The break lies elsewhere: in USERLAND

`libcuda`, `libnvidia-glcore` and the remaining userland libraries are **proprietary blobs**.
They **cannot be recompiled** — and thereby the whole Z16 way ("clone source text and
translate") is barred for them. What they need is **binary Linux ABI compatibility**: glibc
symbols, `ioctl` on `/dev/nvidia*`, `mmap` semantics, `futex`, signals. That is Z14, the strand
that was so far deferred.

**From that follow two ways, and they exclude each other.**

### Way A — with blob: CUDA, but binary ABI

| Stage | Contents |
|---|---|
| A1 | port the kernel module (MIT): PCI/BARs, **large DMA**, MSI-X, load GSP firmware, RPC channel |
| A2 | the `/dev/nvidia*` **ioctl surface** as a PD protocol — the uAPI is the stable side |
| A3 | **binary Linux ABI** for the blobs: loader for foreign ELFs, glibc symbol surface, signals, `futex`, `mmap` |
| A4 | deliver and load 84 MB of firmware → file system, and the **attestation question** (what signs the 84 MB?) |

**A3 is the project**, not A1. And it contradicts the previous line (Z16: translate instead of
driving binaries) — that is a direction decision, no extension.

### Way B — without blob: everything open and translatable, but no CUDA

**`nouveau` (kernel, GPL-2.0) + `NVK` (Mesa, Vulkan driver, open)** — both source text, both
translatable via Z16, no binary ABI needed. Result: **Vulkan/OpenGL, no CUDA.**

## DECIDED: CUDA YES → way A (2026-08-09, Simon)

Thereby the binary Linux ABI (A3) is **no optional strand any more, but a precondition** — and
Z16 ("translate source text instead of driving binaries") is no longer the only line, but one
of two. The blobs are not negotiable: `libcuda` does not exist as source text, and ROCm or
oneAPI would be a different vendor, not a different way.

**What that concretely shifts:** Z14 moves from "assessed, deferred" onto the critical path,
and the most expensive part of the GPU undertaking is **not** the driver.

## Fuchsia as a model — and it is TWO building blocks

The question "can one orient oneself on Fuchsia's layer" hits, and doubly. Both parts
are **BSD-3-Clause**, i.e. permissive — unlike `dde_linux` (GPLv2) there would be **no**
licence friction with AGPL here.

### 1. `Starnix` → the model for A3

A **Linux binary compatibility in userland**: a Zircon process that
implements the Linux syscall surface (`/proc`, `/dev`, `futex`, signals, `mmap`) and drives unchanged Linux ELFs — written
in Rust, TCB-neutral. Exactly the position that a PD would have here.

**The most important transferable lesson is however not the structure, but what Fuchsia had to change at the KERNEL
for it:** *restricted mode* (`zx_restricted_enter`) — a kernel primitive by which a
thread executes foreign user code and whose syscalls **fall back into the userspace handler**
instead of into the kernel. Pure interception from userland (the gVisor ptrace way) was too expensive.

**That is the one line of TCB that way A will cost**, and it should stand in the design before
anybody begins — not as a surprise in the third month. For Caprock that would mean: a thread of a
PD executes foreign code, and its `syscall` entry is delivered to **another PD** instead of to
the kernel. That is a real extension of the thread model, no add-on.

### The fourth precondition, cap-shaped — and thereby MUCH smaller than `restricted mode`

**The proposal (Simon, 2026-08-09): via capabilities of BOTH processes.** It carries, and it is
better than Fuchsia's form, because it needs almost no new mechanics — Caprock has the building blocks
already.

**The core:** the syscall entry of a thread is not *handled*, but **delivered to an
endpoint**. The kernel marshals the trap frame into an IPC message, blocks the
thread, wakes the handler; its `REPLY` writes the result registers back and continues. That
is **literally the fault-handler pattern of seL4** — and `RECV`/`REPLY`, trap frames and the
`switch_to` fastpath all already exist here.

**TCB cost:** one cap slot per thread ("syscalls go to E") plus the marshalling. No new
execution mode, no second context per thread.

#### Who holds what — and the asymmetry is the point

| Side | holds | means |
|---|---|---|
| **Handler PD** | the **endpoint cap** | may receive the guest's syscalls **and write its result registers** — so it can lie to it. Substantial authority, hence a cap and not a global switch |
| **Guest PD** | **nothing** | its authority is *reduced*: it no longer reaches the Caprock kernel at all |

**And the third party that the design must name: who switches over?** Not the handler — otherwise
every PD could pull the syscalls of a foreign thread to itself. Only whoever has
**authority over the thread** may switch over. So the call needs **both**:

    SYS_SETHANDLER(tcb_cap, endpoint_cap)

Exactly that is "capabilities of both processes", made precise: the tcb cap says *whose* syscalls,
the endpoint cap says *where to*.

#### These are NEW cap kinds, no repurposed endpoint (Simon, 2026-08-09)

The first draft above took an ordinary `Endpoint`. That is too little, and for the
reason this project has already paid for several times: **a value that carries two meanings is
harmless only as long as the two happen to be equal.**

The authority here is a **different** one from "may receive IPC":

| | ordinary endpoint | syscall handler |
|---|---|---|
| receives messages | yes | yes |
| writes **foreign result registers** | no — `REPLY` writes the reply, not arbitrary state | **yes** |
| can **lie to** the guest | no | **yes** |
| distinguishable from a service endpoint | — | **must be** |

Hence own kinds:

* **`SyscallHandler`** — "may receive the syscall stream of the threads bound to me and write their
  result registers". **Only of those bound to me**: the binding stands in the kernel per
  thread, and a `REPLY` acts exclusively on the sender. Without this restriction the
  cap would be a master key onto foreign register states.
* **`FaultHandler`** — separate, because `mmap` semantics demands that the handler sees **page faults**.
  A channel with two meanings is the form that has torn here three times (`blocked`,
  the park seam, `CR0.TS`); the fourth need not be.

**What the own type additionally makes possible** and an endpoint does not:

* **Declarable in the manifest** (`CAP_SYSHANDLER`) — who may be a Linux personality stands
  then in the **signed** surface, fail-closed, instead of arising from the possession of an endpoint.
* **Nameable in the audit** — `cdt_audit` can answer the question "who may write foreign registers";
  over an ordinary endpoint nobody can ask it.
* **Mix-up excluded** — a PD cannot accidentally bind its *service* endpoint as a
  syscall handler. The translator rejects it, not a comment.

**The call thereby carries three authorities, each from a different side:**

    SYS_SETHANDLER(tcb_cap, syshandler_cap, faulthandler_cap)

`tcb_cap` says **whose** syscalls (and is held by whoever controls the thread — not the handler),
the other two say **where to**.

#### FOUR ADDENDA BEFORE THE BUILD (objection of 2026-08-09, all four justified)

**(1) This strand REVERSES A RECORDED DECISION — and did not say so.**
**REDEEMED on 2026-08-10 as [Z27](#z27-binary-compatibility-is-from-now-on-a-line--the-reversal-of-z16-decided-2026-08-10)**, together with the demand that the old analysis makes on
the new one (behavioural fidelity, measure for it, LTP as acceptance). The text below stays as
reasoning.
Z16 stands on the sentence: *"Cloning source text and translating strikes binary compatibility — and
thereby the syscall interception, exactly what WSL1 and gVisor failed on."* A
`SyscallHandler` that redirects guest syscalls to a personality PD **IS** the syscall
interception — the WSL1 blueprint with better cap hygiene.
The reversal is presumably **right** (unchanged binaries, Go without libc, customer workloads that
nobody recompiles) — but it is a **new decision against an old one** and belongs with
its reason in the log, not implicitly in a cap design. Otherwise two strands with
contradicting premises stand here, and in three months nobody knows which applies.
**And the old analysis makes a demand on the new one:** WSL1 failed **not** on the
redirection mechanics, but on the **behavioural fidelity** behind it. That does not become one
gram lighter through a cap.

**(2) The authority list is written register-sized and thereby wrong.** Three holes,
ascending:
* **Which registers?** `rax` suffices for `read`. `rt_sigreturn` replaces the **complete**
  trap frame, `clone` needs a **second** frame on a new stack, signal delivery writes
  `rsp`/`rip`. Either `REPLY` may write the whole frame — then the cap is in fact a
  **debugger cap** and "can lie to the guest: yes" still understates — or it may not, and
  the personality **cannot** implement signals and `clone`. **This drawing of the boundary is
  the actual decision and is missing.**
* **The arguments.** Almost every Linux syscall passes **pointers**. The handler must read
  and write guest memory (`copy_from_user` equivalent) — access to the **entire**
  guest address space, wider by orders of magnitude than a result register. Where does it come from:
  mapping of the guest AS into the handler PD (**which cap carries that?**) or a kernel copy service
  (**TCB cost per syscall**)?
* **`mmap`.** The `FaultHandler` **sees** page faults; to **resolve** them means installing mappings in the
  **guest vspace** — a third authority that does not occur in `SYS_SETHANDLER`.

**Honestly added up, the personality PD holds: frame write right, full memory access,
vspace manipulation. It IS the kernel of the guest** and belongs in its TCB — that is fine,
but the entry must **name it so**, instead of writing the authority small. The
manifest declarability thereby becomes **more important**: in the signed surface it then stands who may be
guest kernel, and that is exactly the line an auditor looks for.

**(3) The blocking seam is the FIFTH instance — this time predictable instead of found.**
A guest thread that waits for the handler reply is blocked, with a **new reason**. That
belongs **from day one** in the reason set ([Z24](#z24)), otherwise the park seam repeats itself:
`thaw` wakes a handler waiter, the `unpark` of a sibling consumes the token, and the
guest keeps running **with half a syscall**.
In addition three seams that nobody names today:
* **On Z23:** freezing a personality PD makes **all its guests** unfreezable or
  hanging — the partner naming in the freeze rejection must know the handler wait case.
* **The asynchronous half of the withdrawal rule:** if the handler PD dies, the waiting guest must
  fault **at once**, not wait for ever. "Fault instead of fall back" covers so far only the
  synchronous case.
* **Cycles:** A handles B, B handles A is a **deadlock by construction**. The cheap
  rejection — "threads of a PD with `SyscallHandler` binding must not themselves be bound" —
  must stand **in the kernel**, not in the doc.
  **BUILT on 2026-08-10 (see A3), and NOT in the cheap version:** that would also forbid
  stacked personalities. Built is general acyclicity as a walk over a
  functional graph. The wait reason stands as `BlockReasons::HANDLER` in the reason set.

**(4) The measurement matrix, with the threshold BEFORE.** Round trip against native syscall is only the **base**;
per syscall the handler's memory accesses to guest buffers come on top, and they **dominate**
with `read`/`write` at real sizes. So two loads:
| Load | measures |
|---|---|
| `getpid` (zero arguments) | the base: round trip, marshalling, return |
| `read` with 4 KiB and 64 KiB | the full path **with** guest memory access |
**The threshold from which the cap-shaped version falls stands fixed BEFORE the measurement** — otherwise
afterwards is negotiated what should have stood fixed beforehand.
**And the Fuchsia comparison limps in BOTH directions:** `switch_to` saves the scheduler (in
our favour) — but `restricted mode` also saves the **address-space switch**, because guest and
supervisor share one, and this version switches **twice per syscall** (at
our expense). That is exactly the TLB question from **Z18 (3)**, and the PCID/ASID number from there is the
second half of the calculation.

#### A3 — THE KERNEL PRIMITIVE IS BUILT (2026-08-10). What stands, what is open, and the threshold

**State (2026-08-13): the PAYLOAD stands — a guest makes a redirected syscall and SEES
the result.** Measured in QEMU as check line `redirect : ALL PASS`: a guest issues syscall **39**
(`getpid` in the Linux ABI, a number that the Caprock kernel **does not know** — natively that would be
`ERR_BADCAP`), the kernel redirects, a handler thread in a **PD of its own** reads the
trap frame from the sidecar and writes the reply back, the guest gets `0xc0ffef` = its
own argument + 1. The reply is **computed**, not constant: a fixed number could also be reported by the
handler without having read the frame. And a **decoy** in the reply message
proves that the value did not come via the IPC transport (the guest finds in `x2` its own value
again, not the decoy).

Measured: `redirect` **35/35** host tests, `tools/redirect-negativ.sh` **11 mutations** + 1
source guard with speak check **ALL PASS**, `== HOST-TESTS: ALL PASS ==`,
scheduler model fidelity **29 self-test cases** green, kernel boundary, admission green,
`tools/abnahme.sh` 13 items green (both architectures built, RAM series 512M · 2560M · 3G · 4G ·
6G, load suite 512M · 3G · 6G, aarch64 suite).

**Three counter-proofs, run and logged** (each mutation → suite → reverted):

| Counter-proof | expected | measured |
|---|---|---|
| (a) the frame is **not** copied into the sidecar | the handler sees garbage | `handler-sah-frame=false` (all five bits 0), `x0=0x0`, **`x2=0xbadbad` — the decoy came through** |
| (b) the result is **not** written back | the guest sees its old register value | **isolated:** `handler-sah-frame=true` (0b11111) stays green, only `gast-sieht-ergebnis=false` (`x0=0x0`) and `register-aus-sidecar=false` fall |
| (c) **speak check:** no redirection at all | must not look like a passed run | `x0=0x1` = `ERR_BADCAP` (the native reply), `bedient=0`, `0 Zustellungen`, and **`kein-rueckfall-auf-die-native-ABI=false`** — the privilege escalation becomes visible |

With (a) two fields fall, and that is **no missing isolation**: the reply is computed from the
read argument, whoever does not see the frame cannot deliver it. A conjunct with
two observers — the same form as M1 in `tools/redirect-negativ.sh`.

**Fail-closed is measured along**, on the same line: after the handler PD is shut down the
guest stays **blocked** at its next syscall (`handler-blockiert=true`, round counter stands), instead of
falling back to the native ABI — with a positive control beforehand (`laeuft-vorher=true`).

##### What stands

The seven finished items stand verbatim in done.md (section "Tidied over from `TODO.md`
on 2026-08-11"). What remains here is only what is open of them — see "WHAT IS OPEN" further
below and the addendum table in the READING AID at the very top.

##### The design decision that Z26 did not have yet: SIDECAR instead of register-write cap

Z26 gave the handler a cap with which it **writes foreign result registers**. What is built is
something else, and the reason is **measured**:

| | Words | Bytes |
|---|---|---|
| `caprock_abi::MSG_WORDS` | **4** | 32 |
| `TrapFrame` x86_64 (15 GPR + vector/error/rip/cs/rflags/rsp/ss) | **22** | 176 |
| `TrapFrame` aarch64 (x0..x30 + ELR + SPSR + SP_EL0) | **34** | 272 |

Four words structurally cannot carry `rt_sigreturn` (replaces the **whole** frame) and `clone` (needs a
**second** one) — that is no inconvenience, but the boundary that addendum 2 named as "the actual decision, and it is
missing". So the frame lies in a shared window, the **sidecar**, one slot per bound guest (512 B, derived from 272
rounded up to the next power of two), slot allocation as a **mask** (not as a counter — a
counter would allocate a slot twice after an individual release, and then a guest reads the half
syscall of another).

**That is the form of Fuchsia's `zx_restricted_bind_state`, and it halves the first of the three
authorities from addendum 2:** there is **no** operation "write the registers of thread T".
There is a window in which the frames of **its own** guests lie. The difference is
auditable (a region in the memory bookkeeping instead of an ability in the cap audit) and the
restriction "only threads bound to me", which Z26 demands as a kernel check, is thereby
**structural** instead of checked.

**What the sidecar form cannot do, and it is exactly one thing:** it needs a slot per
bound **guest thread**, not per handler thread. With Fuchsia the sidecar belongs to the thread
that enters `restricted mode` — there is nothing to re-hang. Here several guests hang on one
handler PD; one window for all would be a race between two simultaneous guest syscalls. Price:
memory and a bound (64 slots per handler PD, `ERR_NOSPACE`), not re-hanging.

##### The authority calculation, honestly: the primitive grants ONE AND A HALF of four

| Authority (addendum 2) | here | Consequence |
|---|---|---|
| read **and write** frame | **yes**, whole frame via the sidecar | `rt_sigreturn`, `clone`, signal delivery are thereby **possible** |
| guest **memory** (pointer arguments) | **no** | **every syscall with a pointer is not implementable** — `read`, `write`, `openat`, `ioctl`. For CUDA that means: the whole `ioctl` surface is missing |
| manipulate the guest's **vspace** | **no** | **`mmap`/`mprotect`/demand paging are not implementable** |
| **see** faults | **yes**, own cap | a fault handler *sees*; it cannot **resolve** (that is row 3) |

The two noes are **no oversight**: they are own caps with own design questions (which
cap carries "the whole address space of G"? a kernel copy service costs TCB per syscall). To
take them along would mean making three decisions in one line.

##### THE THRESHOLD — fixed BEFORE any measurement (addendum 4)

**The address-space switch is the open quantity, and it is a design decision, not
negligence.** With Starnix, guest and `starnix_kernel` share **one** address space (lower
half guest, upper half supervisor); a round trip costs there a **mode switch**, no
address-space switch. This version puts the handler into a **PD of its own** and switches **twice per
redirected syscall**.

The Starnix form would be **not cheap** here: Caprock's isolated PDs share the static
upper tables (`ISO_PD_HIGH`, GiB 1..3) — fading a handler in there would give it to **every**
isolated PD (the trap "shared page directories do not tolerate PD-specific entries").
It would demand private upper tables per guest and place the guest's kernel **in its
address space**. Hence: own PD, two switches, price named.

**The threshold from which the cap-shaped version falls and `restricted mode` must be built:**

| Load | Threshold | anchored at |
|---|---|---|
| redirected **`getpid`** (base: no guest memory access) | **≤ 2000 cycles** median | Z18 (2): a Linux syscall lies at 100–200 with mitigations; "below 1000" there meant the service architecture carries. A redirect **is** such a round trip plus marshalling — double is the generous version of the same number |
| **`read` 4 KiB / 64 KiB** | **not yet** fixable | it needs the guest memory authority, and that is not built. A threshold for a path that does not exist would be a number without object |

Above 2000 cycles is **not** "then we optimise": then the design has fallen, and the
answer is called shared address space (private upper tables per guest) or PCID (Z18 (3)).
**The number stands here before it was measured** — otherwise afterwards is negotiated what should
have stood fixed beforehand.

##### WHAT IS OPEN — and that is more than stands above

- [ ] **NEW (2026-08-13): the measurement runs NO ISOLATED handler and no EL0 guest.**
      Both are kernel threads in PDs of their own; the trap path is the same (`int 0x80` creates
      the same frame, `iretq` restores it) and the written-back words are
      the same — but "an EL0 guest is redirected" and "the window lies in the VSpace of an
      isolated handler PD" are thereby **inferred, not measured**. An inferred case
      has already three times been an unchecked one in this project. For the isolated handler the way
      is clear (`spawn_isolated_parked` + `SYS_MAP` on the window cap); it demands handler and
      guest code in `.user_text` and is therefore a round of its own.

- [ ] **NEW (2026-08-13): the write-back covers only the GENERAL-PURPOSE REGISTERS.** `rip`/`rsp`
      (x86_64) resp. `elr`/`sp_el0` (aarch64) do **not** come back — and thereby
      `rt_sigreturn` is still **not** implementable, although the authority table above says
      the opposite ("read **and write** frame — yes, whole frame → `rt_sigreturn`, `clone`
      are thereby possible"). The reason is no oversight: **reading and writing are not the same
      authority.** The frame contains `cs`/`ss` resp. `spsr`, i.e. the **ring** — a handler that were allowed to
      write them back would promote its guest, and that would be the privilege escalation against which
      the switch stands, only from the other side. `rip`/`rsp` are less dangerous, but
      each of them needs a validity check of its own (canonicality — a non-canonical
      `rip` blows up at `iretq` **in the kernel**, alignment). The row in the table has been
      split into **two** since 2026-08-13.

- [ ] **`handler_lebt` reads the wrong quantity — named, not hidden.** What is checked is whether the
      handler **PD** exists and is not shut down. **Not** checked: whether the handler *cap*
      still lies in the cspace. A deleted cap with a living PD leads today to the handler
      no longer `RECV`ing — the guest then waits in `BlockReasons::HANDLER` **instead of faulting**.
      Thereby the fail-closed rule covers the case "cap withdrawn" only by half. A checker that
      reads the wrong quantity structurally cannot see the error; that is why it stands here and
      in the source text at the function.

- [ ] **The asynchronous half of the withdrawal rule is missing** (addendum 3): if the handler PD dies
      while a guest waits, the guest is today **not** woken. It hangs. A path is needed in the PD teardown that releases
      all guests of this handler PD with `ERR_HANDLER_GONE` —
      and the guest list does not exist yet (only the edge `guest -> handler`, not its
      reverse).

- [ ] **On Z23 (freeze): the partner naming does not know the handler wait case.** Freezing a
      personality PD makes all its guests hanging. `handler_lebt` already reports
      `quiescing` PDs as dead (the guest thus faults instead of hanging) — but the
      **freeze rejection** does not name the handler wait case, and that belongs to be decided together.

- [ ] **NEW (2026-08-13), and it is the most unpleasant finding of this round: the whole
      sidecar arithmetic was DEAD CODE.** `slot_offset`, `slot_gueltig`, `slots_in`,
      `fenster_deckt` and `FRAME_MAX_BYTES` had in the **whole tree** no caller outside
      their own test module — measured with `grep`, not conjectured. The mutations M5 and M6 in
      `tools/redirect-negativ.sh` have proved since 2026-08-10 that the **functions** are right;
      that they are **called** was proved by nothing. That is the form "a green line that
      gates nothing", one level deeper: a negative test that secures a property that
      nobody uses.
      Since the payload all five are load-bearing (`slot_gueltig` in `sidecarkopie::slot_adresse`,
      `fenster_deckt` when minting the cap **and** at the binding, `slot_offset` in the deliverer).
      **And in the process a real gap was noticed:** `SIDECAR_SLOTS = 64` and the `len` of the cap were
      **two independent numbers**. A handler cap with a 4 KiB window would have granted 64 slots
      and the kernel written up to offset 32 KiB — beyond the region, into foreign
      memory. Now `install_*_handler_cap` rejects that (`CapError::ZuKlein`) and
      `SYS_SETHANDLER` once more (`ERR_NOSPACE`).

- [ ] **Not measured: the costs — and since 2026-08-13 that is no longer a question of the building blocks,
      but of the machine.** No cycle value, neither for the switch nor for the round trip.
      The round trip was until today **not measurable**, because the payload was missing; now it runs, and the
      threshold (redirected `getpid` ≤ 2000 cycles median) is redeemable. **It is measured on
      a QUIET machine**: this measurement rig has already once been led astray by 3.2-fold vCPU overbooking
      (D13), and a number from there would afterwards stand in the register. The
      switch costs on the unloaded path one
      `Option` read at the running TCB per syscall (one locking, one comparison, no cap lookup) —
      **that too is a claim about the code, no measurement.** It belongs to
      [Z18](#z18-high-performance-for-translated-foreign-code--measured-2026-08-09) (2), which is open anyway
      as "the actual gap".

#### The rule without which it would be a privilege escalation

**If the handler cap falls away (deleted, withdrawn, handler dead), the guest must FAULT — not fall back
to the native ABI.** A fallback would make out of the withdrawal of a cap a **promotion**:
the guest would suddenly speak directly to the kernel. Fail-closed, and it is the same form as
`ERR_SERVER_GONE` at the endpoint exchange.

#### What is still to be decided

* **Faults separate or carried along?** `mmap` semantics demands that the handler also sees
  **page faults**. seL4 separates the fault endpoint from syscall delivery; here a second
  cap would be cleaner than an overloaded channel — a channel with two meanings is the form that this
  project has already paid for three times.
* **The measurement that decides (a) against (b):** cost of an IPC round trip against a native
  syscall on this machine. Fuchsia went to `restricted mode` **because of the costs**; here
  the comparison is however a different one, because `switch_to` on the same core switches directly (with
  budget donation) instead of running via the scheduler. **Measure before the build, not afterwards** — and
  the number belongs next to that of gVisor's ptrace way, so that it is clear which comparison applies.

### 2. `Magma` → the model for A1/A2

Fuchsia's **GPU architecture**: split into a *system driver* (in the driver process) and an
*application driver* (vendor code in the application process, e.g. Mesa), which talk via channels.
So exactly the cut that Caprock needs anyway — command buffers are built in the application
and submitted via IPC, instead of via `ioctl` on a device.

**The benefit lies in the design, not in the code.** Zircon's object model (handles, VMOs, channels) is
not that of Caprock (caps, PDs, endpoints); it is about the **cut**, and that is
transferable. To port Fuchsia would be no shortcut, but a second operating system.

**To be checked before adoption** (from the source, not from memory): the exact scope of
`zx_restricted_enter`, how Starnix maps `mmap`/`fork` onto VMOs, and whether Magma's cut carries the
CUDA surface at all — Magma is tailored to graphics, and `libcuda` talks to
`/dev/nvidia*` and `/dev/nvidia-uvm`, not to a graphics uAPI. **That is the point at which the
model could end.**

## The question that preceded the decision

* **PaaS with GPU compute** (inference, training) → **CUDA is the market**, hence way A, hence binary
  ABI. There is no shortcut; ROCm/oneAPI would be a different vendor, not a different way.
* **Only display/compositing** → way B, and then **i915 is also the cheaper entry**
  (728 of the 900 files MIT, measured).
* **Server without graphics** → no GPU at all, and the whole strand drops out.

## What comes first IN ANY CASE

Independent of the way: **`CAP_IRQ` on x86** (IRTE allocation), **large/contiguous DMA** and
**firmware loading from a file system**. Without these three no variant is runnable — and all
three stand on the way to the server anyway.

**Still not the next step** — but the decision has been made, and it costs the
difference between "translate" and "drive foreign binaries". The three preconditions (`CAP_IRQ`
on x86, large DMA, firmware from a file system) stand on the way to the server anyway; the
fourth — **the kernel primitive for redirected syscalls** — stands on no other way and is
thereby the honest price of this decision.

### Z23. Process freeze — THE GROUP CUT STANDS (2026-08-21)
**Class:** side strand · **Precondition:** several threads per PD (Z22 P2) — **fulfilled**, the probe
runs three threads in one PD

> **Register correction 2026-08-21, second round.** The entry said "S1 is built", and the
> actual state above it said "there is no process freeze, and the obvious version is
> **structurally impossible**". Both were true on 2026-08-09. Measured on 2026-08-21: **S1b, S2, S3
> and half of S6 are built and gated** (`pdfreeze : ALL PASS`, **27** conjuncts, seven
> counter-proofs). What is open stands from **S4**.

**What dissolved the impossibility was no mechanics, but a different QUESTION.** `freeze_thread`
asks "does this thread hold an open relationship?" and rejects a mutually calling pair
of the same PD twice — there is no order that resolves that. The cut asks "**does the
relationship leave the cut?**". If both ends lie inside, it is no open relationship of the cut;
the reply token stays untouched, and after thawing the transaction continues. Exactly that
is the case that Z4a does not show yet — and the probe proves **both** halves on the same pair
in the same run: `einzeln-unfrierbar=true/true` (each names the other) beside `umfang=3`.

Rule and decisions stand normatively in `docs/invariants.md` §1d, the migration sequence in §11.3.

- [x] **S1 — built.** First **close the gates**, then freeze.
      A `PD_QUIESCING` bit per PD, checked in the syscall path: `CALL`/`RECV` **out of** the PD
      fail with `ERR_QUIESCING`, running transactions may **finish** (`REPLY` stays
      allowed) — the same gate logic as A-4.2, only with the PD as scope instead of one endpoint.
      **Why not simply shut down all endpoints of the PD:** at an endpoint also hang
      **foreign** PDs. An endpoint-wide latch would freeze third parties along — the shutdown must hang on the
      **subject**, not on the object.
      **TCB cost, named:** one bit per PD and one check in `CALL`/`RECV`. Nothing more.

- [x] **S1b — decided and measured: `ERR_QUIESCING`, but ONLY at the exclusive channel.**
      The cut locks an endpoint inward (`begin_quiesce`) **exactly when the
      frozen PD serves there alone**. Foreign callers thereby get "comes back soon"
      instead of unbounded blocking; where a foreigner also serves, it is **not** locked —
      there the latch would freeze third parties along, and the foreigner continues serving the callers anyway.
      **The price stands with it:** a client at an exclusive channel needs retry logic.
      The two rejected versions are named (bounded queueing — nobody pays for the
      buffer, and the overflow is again a D11 case; unbounded blocking — honest for
      a PaaS, but passes the deadlock on to bystanders).
      Measured via `gate_new_transaction` — **the same** function that `call`/`recv` execute,
      no replica that can drift apart. Conjuncts `kanal-zu` and `empfaenger-gezogen`,
      **separate**, because they are two promises; counter-proofs M3 and M4 show them individually.

- [x] **S2 — the rejection names the PARTNER, and the deadline has a named outcome.**
      `Freeze::BusyOn(Quiescence, ThreadId)` for the individual case, `PdFreeze::Deadline { tid,
      partner, q }` for the cut. A waiting **receiver** has no partner, and to
      invent one would be worse than to name none — then one would call the wrong one to account.
      **To whom the name goes stands in the code:** to the holder of the freeze authority, via the
      kernel path, **not** via a syscall — an error message with a foreign thread name is an
      oracle for the IPC topology of other PDs.
      Measured as `frist-nennt-partner` at a PD that calls into a third one that never
      answers; in addition `frist-tore-auf` — **every** outcome except `Frozen` leaves the state
      that it found.

- [x] **S3 — the freeze is a SET, no sequence.**
      `FrozenPd`: `#[must_use]`, **no** `Drop` (otherwise the content could not be moved out
      on thawing), **no** public way to the `ThreadId`s — whoever could touch individual participants
      could halve the set. `abort_freeze` beside `thaw_pd`, with the same
      body and a separate counter: the two **mean** different things.
      **That the way back is the same is the result of Z24** — with `BlockReasons::FREEZE`
      as its own reason (bit 7, eighth instance) the abort is *one* `remove` per participant, without
      intermediate state and without foreign wake token. Exactly for that reason the register said "Z24 before Z23".
      **`PAUSE` would have been the obvious and wrong choice** — its doc even named the
      group cut as a future user: a single `RESUME` on a participant would release
      it from the group, the PD would be half frozen, and every checker would report order.
      Measured as `resume-wirkt-nicht`; counter-proof M1 restores exactly that.
      **All rejections are named and fail-closed:** `HasDma`, `TooManyThreads`, `MultiReceiver`,
      `SenderQueued`, `Debugged`, `AlreadyQuiescing`, `Empty`, `Deadline`.

- [ ] **What that means OUTWARD, and it belongs in both protocols.** "The checkpoint carries
      no thread" means: **resume latency and live migration currently have no measurable
      object.** What exists is an **application** checkpoint (progress counter +
      cap classification); a latency number on it measures something different from what "resume" in a
      PaaS promise means.
      **The two-phase group cut must be specified as a design BEFORE benchmarks are named
      outward** — otherwise the number arises first and retroactively defines what it
      is supposed to have measured. The sentence that is missing for it is the order **with its
      failure cases**: what happens to a transaction that does not expire — deadline, abort, or
      inheritance into the checkpoint? Three different promises, and none of them has been made.
      (The same honesty as "*a thread survives a boot boundary* reads stronger than the
      matter is". **Belongs also in the Velve protocol**, not only here.)

- [x] **S4 — the state is ENUMERATED, and the confidentiality rule stands beforehand.**
      No new method: the same rule one level deeper. `checkpoint::classify_thread_part`
      beside `classify`, `ThreadStatePart` as a **closed** enumeration — a new TCB field
      that is missing is noticed at the `match` and not at the first thaw with wrong registers.
      Enumerated are: registers (**with the ring**), **FP state** (in the `FP_STATES` slot, *not*
      in the frame — without it a thread travels without its XMM), stack, EL0 kernel stack, priority,
      MCS account, reason set, **park wake token** (the debt from Z22 P4 — it is no reason,
      but a token, and therefore falls through the grid when enumerating the reasons),
      reply token, notification badges, handler binding, core affinity, cycle stamp.
      **Rejected by name** instead of silently omitted: `LocalReason::MachineLocalNumber`
      as **its own** reason — a device window is *not producible* over there, a core
      affinity *arises anew over there*; the same code would let somebody search for a device
      where an allocation is missing. Relationships (reply token, handler binding) hang on the `Scope`,
      literally as with the caps.
      **The confidentiality decision is made, machine-readable and measured:**
      `image_is_confidential`. An `Image` with `progress` was a piece of metadata; one with registers and
      memory is a **data carrier**. Measured consequence: **every complete image is
      confidential** — there is no migration case in which the storage and transport rule falls away.
      6 new host tests (38 in `checkpoint`), each with both directions. Normative in
      `docs/invariants.md` §1d.2.
      **What this is NOT:** a serialiser. `Image` still carries no thread state — the
      enumeration is the precondition for there being one, and the honesty about it
      stands unchanged in the next item.

- [ ] **S5 — device and DMA: the first version stands, the right one does not.**
      **Built:** fail-closed — a PD with a DMA cap is **not frozen at all** (`PdFreeze::HasDma`,
      checked before any other step, so that a driver PD is not half treated). A
      rejection that is right is better than a promise that does not hold.
      **Open and unmeasured:** the domain swing (the IOMMU domain of the device to non-present,
      any DMA during the freeze becomes a **fault** — loud, measured, area-wide instead of
      word-sized) and the `PD_FREEZE_PREPARE` upcall (driver-specific, belongs **in** the PD, hence
      TCB-neutral). The way from fail-closed to the final state runs via a line that already
      exists.
      **The canary word remains — but as a tripwire in the self-test that checks the CHECKER, not as an
      assurance.** It catches only the device that happens to write this one word; DMA into all
      other pages would remain invisible, and the false reassurance would outweigh. "No DMA during
      freeze" belongs at the hardware boundary that can enforce it completely.
      **The rejection has been MEASURED since 2026-08-21 — with a positive control.** The same PD twice:
      with the DMA cap `HasDma`, after `clear_cap` it passes (`dma-abgewiesen`,
      `dma-ohne-cap-geht`). Only the second half turns the result into a statement about the
      **cause** — a rejection alone proves only that something did not work.

- [x] **S6 — thawing is not the reversal, and both halves stand.**
      Receivers are **re-enqueued before** opening the channels (the other way round a
      caller would run into the sender queue instead of being served), and `thaw` removes `FREEZE` and
      **only** that (`klient-laeuft-nicht`, counter-proof M7).
      **The park seam is measured, in all three directions:** an `unpark` during the cut
      does **not** let the thread run (`park-stumm-im-schnitt` — the reason `FREEZE` still
      stands, and enqueueing happens only on an empty set); with a token it runs after the thaw **at once**
      (`park-marke-ueberlebt`); without a token it stays lying (`park-ohne-marke-bleibt`). Without the
      third the second proved nothing.
      **The old finding beneath it has fallen away with Z24, not been fixed:** `thaw_thread` called
      `unblock` ("lift any block"); since the reason set it calls `resume` and removes
      `PAUSE` and only that. The described chain (thawed parked thread keeps running
      with `parked == true`, the next `unpark` consumes the token into the void) is thereby
      **no longer formulable**. The entry at the very bottom still stands — it describes a state
      from before Z24 and belongs re-checked instead of believed.

- [x] **TIME — decided, built, measured. And the gap in it is named.**
      **The monotonic clock does NOT pause along.** Three reasons, and the third alone decides:
      a clock per PD would be a second memory for one fact (MCS budget, period and
      cycle stamp run on the global one); a time stamp of a **non**-frozen PD received via IPC would lie in the
      future of the own clock — "the clock jumps" would become "foreign
      time stamps are unusable"; and **EL0 reads the clock directly** (`rdtsc`/`CNTVCT_EL0`), a
      pausing kernel clock would not be covered by the hardware clock at all. The kernel cannot redeem this
      promise — so it does not give it.
      **Instead the duration is REPORTED:** `thaw_pd` gives `Thawed { threads, ticks }`, to the
      holder of the freeze authority — the same rule as with the partner name. Measured as
      `standzeit-berichtet` against the observation window of the cut (`standzeit=22` at four
      ticks of window); a 0 could not be told from "not measured".
      **Open, and it is no side effect:** the frozen PD itself does not learn the duration.
      For that a self-information syscall is needed. A thread that computed a deadline from `rdtsc` before the freeze
      **cannot** correct it afterwards. A counter without a reader would have been the worse solution here
      (dead sidecar arithmetic) — hence first the report path that exists already, and the ABI half as an item of its own.

- [x] **Acceptance — run, and it can fail.** `pdfreeze : ALL PASS`, 21 conjuncts,
      **both** architectures. The five demanded statements are individually readable:
      1. `laeuft-vorher` (speak check: a counter that never ran also stands),
      2. `steht` over **four ticks** and `laeuft-danach`,
      3. `einzeln-unfrierbar=true/true` beside `umfang=3` — **the case that the order cannot
         do**,
      4. `frist-nennt-partner` — rejection with name instead of hang,
      5. `kanal-zu` + `empfaenger-gezogen` — the defined behaviour for third parties, measured.
      In addition S5's rejection with positive control, S6's park seam in three directions and the reported
      standing time (see there).
      In addition `tools/pdfreeze-negativ.sh`: nine mutations, each with the name of the conjunct that
      must fall, and with the conjuncts that must **stay green**.
      **M5 does NOT isolate, and that stands in the script instead of being concealed:** the
      partner information carries two statements at once (name the rejection **and** recognise an internal
      relationship as internal), so its removal tears both.

- [ ] **To be fixed beforehand, because Z23 otherwise plans on a broken foundation** (finding of
      2026-08-09, read from the code): `parked`/`park_wake` are read **outside the scheduler by
      nothing** — not by `thread_quiescence`, not by the checkpoint, **and not by
      `thaw_thread`/`PDCTL RESUME`**. From that follows already today:
      * `thaw_thread` calls `unblock`, and `unblock` does not touch `parked` → a thawed
        parked thread keeps running with `parked == true`, and `is_parked` lies from then on.
      * The next `unpark` sees this stale `parked == true`, **consumes the token
        at once** and calls `unblock` on a running thread. The wake token is thereby gone, without
        anybody having slept — **a lost wakeup**, exactly what Z22 P4 is built against.
      * Conversely the `unpark` of a sibling thread lifts a `PDCTL PAUSE` on a parked
        thread **silently**.
      Again "one bit, two reasons" (D9), this time at the seam between park and pause/resume: in
      `unpark` avoided, left standing at the other side. **And it shifts the status of the
      spurious-wake contract:** the system already produces spurious wakeups today, the contract
      is thus no precaution, but the only version that is right.

- [ ] **Three probes of this strand gate only via the suite `check`, not via `all_done()`**
      (finding of 2026-08-25, read from the build warnings when attaching `ckptcut`, not
      conjectured): `dbgprobe::urteil()`, `pdfreeze::urteil()` and `ckptcut::urteil()` are
      **all dead** — rustc reports them as `never used`. The printed line is thus read only by
      `grep` in `test-qemu-x86.sh`/`test-qemu.sh`; in the kernel itself nothing hangs on it.
      That is the weaker of the two layers, and the CLAUDE.md note of 2026-08-07 decided
      exactly about that ("Now the list is the verdict, on both branches").

      **Why it is nevertheless not fixed with one line, and that is the actual point:**
      all three probes have SKIP outcomes (no PD free, no endpoint, no page). Taken into
      `all_done()` a resource SKIP becomes a **watchdog** — the run then looks like a hang, and that is exactly the
      indistinguishability that has already cost this project days twice. What is needed is thus a third value ("not run")
      beside true/false, or a SKIP that lets `all_done()` pass and still stands in the report
      as SKIP. The decision belongs before the wiring.

- [ ] **RULE CANDIDATE: every number with a promise behind it needs a CONJUNCT beside it.**
      The third form of the same family as `all_done()` and the speaking line: where a result
      flows into a **number** instead of a **predicate**, it can fall without anything turning red.
      Measured on 2026-08-21: the `dbg` line stood at all `true` and its verdict was red — the
      fallen summand (`schlimmstfall_ok`) was in a number, the conjunct was never printed.
      Affected are at least: latency p99, worst-case sum, standing time of the cut,
      cycle budgets, `NOSEL_TEXT`. **The number in the report is diagnosis; the predicate is the
      check** — both belong printed, otherwise the next one searches at the wrong place.
      Next step: extend `tools/berichtsgatter.sh` by exactly this question (does a
      line print a number with a threshold without printing the associated conjunct?).

- [ ] **`CapSpace::inspect` computes `child_count` — a CDT walk, and the caller does not know it.**
      `kind_of` has solved the freeze case; the structural question is bigger: **may an
      information function make a walk on the side?** Today all five remaining callers are
      single queries (`system.rs` 2493/9511/9552/9694/9711) — the next loop caller falls
      into the same trap. To be decided: take `child_count` **out of** `inspect` and keep it as an operation of its own
      (then every site that really needs it costs one line — the effect
      of the exhaustive rework, not its price), or **name** `inspect` as "expensive" and
      put a guard against calls in loops. The second version is the weaker:
      a guard that counts call sites checks the style.

- [ ] **S5 swing: the rejection is measured, the SWING has no positive control — none at all.**
      For `HasDma` it stands (the same PD with and without cap); for the domain swing it is missing
      completely, and the text must not blur that. **And it needs no real device:**
      an IOMMU domain with a deliberately invalid mapping and something that
      writes over it (QEMU `virtio` or a dummy BDF) suffices to see that the **fault** comes
      instead of the write access. Thereby the swing is measurable before a driver PD needs it.

- [ ] **Side finding 2026-08-21: `tools/kernel-grenze.sh` is RED — and it stands in `abnahme.sh`.**
      Five HAL modules stand neither on the allow list nor as a named exception:
      `fbtext`, `iommu_health`, `numa`, `bootparams`, `smt` (41 HAL modules checked). All five
      are work of the last days from this branch; none of them belongs to the group cut.
      **Not silently admitted**, and that is the point: the guard demands a
      reason why the KERNEL itself needs the module — and for `fbtext` (text output on a
      framebuffer) the obvious answer is "belongs in a userland driver PD (A-5.1)", not
      "onto the list". Exactly for that the list exists; filling it in passing makes of it
      the same text surface that the identity guard was in its first version.
      To be decided are five individual cases: `numa`/`smt`/`bootparams` are **platform information**
      (SRAT/SLIT, CPUID topology, boot parameters) and thus presumably kernel; `iommu_health`
      belongs to the isolation mechanics; `fbtext` is a driver.
      **And the question above it is the same as with the model-fidelity guard:** a gate that stands in the
      acceptance and is red was either not run for days or its result was not
      read. Both are a finding about the acceptance, not about the five modules.

- [ ] **Side finding 2026-08-21, and it is older than this strand: the model-fidelity guard of the
      scheduler ran in NO gate.** `tools/verus-modelltreue-sched.sh` hangs solely on
      `tools/verus-verify.sh`; the acceptance runs `host-tests.sh`, and there only the
      IPC guard ran. Measured on this day it reported **three** unnamed write sites from Z6b
      (`debug_stop`, `debug_continue`, `debug_release_pd`) — red for two days, without anybody
      seeing it. **A gate that exists and is not run is none**; the same class
      as `.gitea/workflows` on a GitLab server and as "aarch64 was built and never
      booted".
      **Fixed:** the three are entered, `freeze_group`/`thaw_group` in addition, and `host-tests.sh`
      calls it along as target `schedtreue` (it needs no Verus, only Python).
      **What remains open is the question above it:** which of the other guards likewise hang only on
      `verus-verify.sh`? That is an inventory, no conjecture — and it belongs to be done before the
      next red guard stays unnoticed for two days.

- [ ] **Side finding 2026-08-21: `release_finalized_debug` takes `CAPS` INSIDE `SCHEDS`.**
      The function doc says expressly "`CAPS` is released, the order is
      `CAPS < EPS < SCHEDS`" — and the `pd_of` closure that it passes to `debug_release_pd` calls
      `pd_of_thread`, i.e. `CAPS.read()`, **while `SCHEDS[c]` is held**. That is R0
      inside R2, i.e. the reversal of the order from `docs/invariants.md` §1.
      **Harmless today, and for a reason that is written down nowhere:** no path
      takes `SCHEDS` inside `CAPS` (checked at all seven long-lived
      `CAPS.write()` bindings in `system.rs`). Exactly the form "a danger that is warded off by hand at one
      place" — the edge `CAPS -> SCHEDS` would be an ordinary, plausible rework,
      and the deadlock would then hit a place that has been green for weeks.
      `freeze_pd` therefore does it the other way round (`live_thread_ids` collects under `SCHEDS`, filtering
      happens afterwards under `CAPS`); `release_finalized_debug` belongs to be brought in line — or the order
      belongs expressly extended by a permitted exception. Silently it does not work.

### Z22. The four hard places from Z21 — built
**Class:** substrate · **State:** 2026-08-09 · Guideline: *as much as possible in the PD, TCB as
small as possible, performance maximal.*

The division that follows from it:

| Item | in the kernel (TCB) | in the PD |
|---|---|---|
| 1 IRQ | IRTE allocation with SID check, vector→notification | masking at the device (own MSI-X table), handler |
| 2 threaded IRQ | **nothing new** | workqueue, exclusion between own threads |
| 3 DMA | map region **once** (already stands) | IOVA allocator over the window, `dma_map` without syscall |
| 4 sleeping | one syscall + two bits per TCB | wait queues, completions, `wait_event` |

- [~] **P1 — x86 MSI-X + IRTE: encoding AND allocation stand, nothing is wired**
      (2026-08-09 encoding, allocation merged 2026-08-10).
      **New since the merge:** `vergib`/`ruecknahme` with `SVT=01`+`SID`, in addition seven mutations in
      `tools/irte-vergabe-negativ.sh`, each with the NAME of the test that must fall.
      **What continues to be missing, and it is the whole point:** no kernel path calls them. The IRT
      has stood at all "not present" since B-3.2, every driver polls. A module that is called only by
      its own tests is built and not in operation — and this
      distinction belongs in the entry, otherwise `irte : ALL PASS` reads like an operational statement.
      **Done:** `crates/caprock-hal/src/x86_64/irte.rs` — IRTE and MSI address encoding as a
      **pure function**, 10 host tests, hooked in as target `irte`. On the model of `dmar.rs`
      an own file, and for the same reason: a bit in the wrong place manifests as
      *"the device simply does not interrupt"* — without a fault, without a message, without anything that
      looks like an error. With literals checkable in seconds; in QEMU it would need device,
      driver and luck.
      The security statement is `SVT=01`/`SID`: the PD writes its MSI-X table **itself**
      (it owns the window, which saves a syscall per vector) — and could thereby enter the handle
      of a **foreign** IRTE. The source check of the unit rejects that. Without it
      that would be a hole that is never noticed in normal operation.
      Three rejections instead of silent truncation: vector < 32 (a device on vector 14 would look like
      a page fault), APIC ID > 255 without `EIME` (truncated it would point to a **different**
      core — an interrupt that silently arrives at the wrong place looks like success), and a device number > 31
      (would run into the bus number and assign the entry to a **foreign**
      device).
      **Open:** the allocation (index allocator + writing into the table + `IEC` invalidation),
      the way from the manifest to the IRTE, a `SYS_MSI`-like access or an allocation in the loader,
      and the delivery up to a driver PD. In addition the three x86 no-ops in `intc`.

- [~] **P2 (state of 2026-08-09, outdated) — the userland half stands, the threads are missing.**
      **Done:** `crates/caprock-wait` — mutex, `WaitQueue`, `Completion` over a trait with
      **two** methods (`park`/`unpark` from P4). 11 host tests against a stand-in. The
      uncontended path is **zero syscalls**; the full wait room is **named** and the caller
      then does not park (D11); the self-deadlock is reported instead of hanging; `Completion`
      **counts**, because a `complete()` before waiting is the normal case in a driver.
      **Open:** several threads per PD. The cheapest way is the **manifest** (the loader creates N,
      no new syscall, kernel costs **nothing**) — the 96-byte entry is however full, it
      would need an `entry_len` bump. The format is laid out self-describing for it; it
      however touches the **signed** surface and deserves a pass of its own.

### Z21. Linux drivers as PD processes — assessed 2026-08-09
**Class:** compatibility layer · **Effort:** large, but **once instead of per driver** ·
**TCB:** neutral

- [ ] **Possible? Yes, and it is proven — no rewrite, but a BUILD.** Genode runs
      Linux drivers in userspace components (`dde_linux`/`lx_emul`), Intel graphics included;
      rump kernels do the same for NetBSD. Decisive about the approach: **the real
      Linux sources are translated**, against an emulation layer. That is why it scales to many
      drivers, instead of replacing one.

- [ ] **What the layer must deliver** — the list is long, but finite:
      memory (`kmalloc`/`vmalloc`, page allocator, **DMA API**), synchronisation (spinlocks,
      mutexes, completions, wait queues, RCU), time (`jiffies`, `ktime`, timers, delays),
      concurrency (`kthread`, workqueues, tasklets), **interrupts** (`request_irq`, threaded
      IRQs), PCI (configuration space, BARs, MSI/MSI-X), device model (`struct device`,
      probe/remove), `request_firmware`, `readl`/`writel`/`ioremap`.

- [ ] **Four hard places that are specific to a PD — and three of them are work, not
      doubt:**

      1. **`CAP_IRQ` is ineffective on x86** — **corrected on 2026-08-09, looked up instead of
         remembered.** The entry said "defined in the manifest format, not implemented". That is
         false: the whole path stands (`install_irq_cap`, `bind_irq`, a **lock-free** hook in the
         IRQ context that masks and notes, and `drain_pending_irqs`, which delivers outside the
         IRQ context as a notification badge) — and it **runs on aarch64**, with a
         check line (`RTC_INTID`, badge, `irqs_delivered() >= 1`).
         Ineffective it is on **x86**, and at a narrowly delimited place: `enable_intid`,
         `mask_intid` and `route_spi` are there documented **no-ops** ("IOAPIC not yet
         ported"). So what is missing is not the mechanism, but the x86 delivery — and for
         PCIe the right way is **MSI-X**, not the IOAPIC. In addition the IRTE allocation (B-3.2 has
         set the table to "not present", that is intentional).
         **Second gap that the entry did not name:** `bind_irq` is **kernel-internal**. A
         driver PD cannot bind its IRQ itself — the test does it from the kernel side.
      2. **"Lock interrupts" is meaningless in a PD.** `spin_lock_irqsave` in Linux is
         exclusion against the *own* IRQ handler. In a PD it becomes exclusion between
         **threads of the same PD** — so it needs threads-in-PD (Z19) and the mapping
         "handler = thread", which is exactly a *threaded IRQ*.
      3. **`virt_to_phys` versus IOVA.** Linux drivers compute with physical addresses. Here `Pa` and
         `Iova` are **separate types** — the mix-up that this project has deliberately made
         impossible is the normal case in Linux drivers. The DMA API must be mapped onto the DMA cap.
         Rather an advantage (the separation exists), but work at every site.
      4. **Drivers sleep.** `msleep`, `wait_event`. In a PD that means IPC/notification —
         doable, but determines the structure of the whole layer.

- [ ] **The price that does not stand in lines.**
      * **Maintenance:** Linux's *internal* API changes with every version. Genode pins kernel states
        and pulls in periodically. These are **permanent costs**, no one-off costs — the same sort
        that spoke against a fork at [Z17](#z17-turso-as-a-native-database--pre-measured-2026-08-09-not-begun), but here is
        unavoidable.
      * **Licence:** Linux drivers are GPLv2, this project is **BSD-2-Clause**. The PD separation
        is here an **advantage**: driver code in a process of its own behind an IPC boundary is
        something different from linked into the kernel binary. The emulation layer itself would be a
        derivative work and thus GPL — cleanly separable, that is how Genode solves it too. **No legal statement,
        but the architecture stands on the more favourable side.**

- [ ] **Mesa: recompile YES, port NO — and the reason supports the whole design.**

      Mesa **already** runs in userspace. The Linux separation is:
      kernel = i915/xe (modesetting, memory management, command submission) ·
      userspace = Mesa/`iris` (translate shaders, build command buffers) → `ioctl` on `/dev/dri/cardN`.

      **The DRM uAPI is stable and documented; the internal kernel API is not.** The
      PD boundary thus falls exactly where the interface is stable — the unstable side
      lies *inside* the layer. That is no coincidence, but the same cut that Linux
      itself draws.

      Mesa thereby needs:
      * **Recompile** against the Caprock libc — like everything in Z16, no special case.
      * `ioctl` → **IPC**. Exactly the form that `virtio-blk` already has today: op codes via an
        endpoint (`OP_INFO`/`OP_READ`/`OP_WRITE`/…). The ABI of this kernel knows no `ioctl`
        (0 occurrences) and needs none.
      * `mmap` of GEM buffers → memory caps + `SYS_MAP`.
      * fences/sync → notifications.

      **Mesa is thus not rewritten**, and this part also does not age with every
      Linux version.

- [ ] **Sensible? Conditionally yes — and the condition is the order.**
      * For the **server** it is **not** the next step. NVMe (~1500 lines, written
        ourselves) and the RTL8168 are cheaper than a Linux layer, and a server needs
        no GPU.
      * For the **desktop** it is the only realistic way — rewriting i915 is none.
      * **The turning point:** as soon as more than two or three non-trivial Linux drivers are needed,
        the layer is cheaper than the individual ports. For WiFi (MT7925, `mac80211`)
        alone it presumably would already be.

      **Recommendation: not now.** But **`CAP_IRQ` is a precondition for both** — for own
      drivers as for borrowed ones — and therefore belongs to be brought forward, independent of how this question
      is decided.

### Z20. What is missing up to a usable SERVER OS — and what a desktop would cost
**Class:** classification · **State:** 2026-08-09 · Measured values are measured, estimates are marked as
such.

- [ ] **Server OS: reachable, and the existing plan covers the larger part.** Z19 delivers the
      substrate, Z16 the libc, Z14 the memory server. **What is missing beyond that and today
      stands in no item:**

      | missing | Size (estimate) | TCB |
      |---|---|---|
      | **File system beyond FAT16** — directories, paths, VFS; FAT16 with *one* file is no server file system | large, but dependency-free like `caprock-part`/`-fat` | neutral |
      | **TCP/IP** — `smoltcp` in a PD (Rust, `no_std`, proven) | weeks to run, months to harden | neutral |
      | **NVMe driver** — `virtio-blk` runs only in VMs | small: queue pairs, admin + IO queue, PRP lists | neutral (PD) |
      | **Realtek RTL8168** — the network card of this laptop, classic and documented | medium, far cheaper than WiFi | neutral (PD) |
      | **Standalone boot on bare metal** | GRUB ISO stands; open are ACPI/APIC on real chipsets | — |
      | **Service management, logging, configuration** | medium | neutral |

      **The actual state on real hardware is better than "never".** The core hand-over
      (`tools/handover/`, stages 0/1a/1b proved) takes five E-cores offline and sends them
      INIT-SIPI-SIPI — Caprock has already executed instructions on this bare metal, only not
      standalone.

      **Estimate, expressly as such:** a *demonstrable* server — statically built
      Rust/C binaries, HTTP via `smoltcp`, data on NVMe, booted standalone — lies at
      **6–12 months** of focused work for one person. *Usable* in the sense of "a stranger
      operates it" rather **1–3 years** with a small team. What carries the estimate: every item above
      is **bounded and TCB-neutral**, and exactly one line in the whole undertaking needs the kernel.

- [ ] **Desktop with KDE on this laptop: two to three orders of magnitude more — and not the same
      kind of work.** That is the point: the volumes lie where measuring and a small TCB do
      **not** shrink them.

      **CORRECTION (2026-08-09, after objection): the comparison "i915 is 1.9 MB, the TCB 236 KiB"
      was skewed — and by this project's own logic.** A graphics driver belongs in
      a **HardwareLand PD**, exactly like `virtio-blk` and `virtio-net`, and then the TCB grows by
      **zero**. The same calculation as with the WASM engine (216 KiB of code, as large as the kernel,
      but in a PD). A crashing graphics driver then takes down its PD and not the system —
      that is an **advantage** over Linux, where a GPU driver error is a kernel error, and
      it is the product thesis in the hardest possible case.

      The architecture for it stands to a large part: MMIO window on the own
      configuration-space page, DMA region with its own VT-d context and its own IOVA window,
      device allocation from the manifest, and the separation of two driver PDs is **measured**
      (A-5.4). What is missing is not isolation, but:

      | Building block | the actual difficulty |
      |---|---|
      | **`CAP_IRQ`** | defined in the manifest format, **not implemented** — every driver polls. For a GPU not viable. Needs IRTE allocation (B-3), and the remapping table has stood intentionally at "not present" since B-3.2 |
      | **The Linux driver API** | i915 cannot be lifted out alone: it hangs on DRM core, GEM/TTM, dma-buf, workqueues, PCI subsystem. Porting means building a **layer** that executes Linux driver code without Linux |
      | **Firmware** | load GuC/HuC blobs — needs a file system |
      | **Amount** | ~150k lines for i915 alone, TCB-neutral or not. Neutral does not mean cheap |

      **The precedent, and it makes the matter more plausible than I presented it:**
      **Genode** runs Linux drivers in userspace components (`dde_linux`), Intel graphics
      included. That is proven, not hypothetical.

      **And it changes the economics:** the layer is the investment, the drivers are
      comparatively cheap afterwards — it unlocks **all** Linux drivers, not one: WiFi,
      GPU, audio, USB. Whoever wants the desktop does not build i915, but `dde_linux`. That is an
      own strand of the order of Z16, and it has the same character:
      **compatibility layer in userland, kernel untouched.**

      | Building block | why the desktop nevertheless remains expensive |
      |---|---|
      | **Escape framebuffer** | EFI GOP without acceleration is doable — without 3D, without video decoding, without mode switching, without external monitor, without hotplug |
      | **Qt6** | ~5 million lines, needs full POSIX plus fontconfig, freetype, harfbuzz, ICU, D-Bus, Wayland. Portable (QNX shows it), but a project of its own |
      | **KDE Frameworks + Plasma** | once more millions of lines above it |
      | **WiFi MT7925** | 802.11be, very new; needs firmware and builds on `mac80211` (~200k lines) |
      | **Input** | xHCI + USB HID + I2C HID (touchpad) + a libinput-like layer |
      | **Suspend/resume, battery, thermals, brightness** | touches **every** driver — no feature, but a property of the whole system |

      **And it does not serve the product thesis.** The thesis is a cloud for Vercel-like services,
      isolation by the kernel instead of by VMs. A desktop would be a hobby milestone.

- [ ] **The interesting middle, shared with the server path: HEADLESS on real bare metal.**
      NVMe + RTL8168 + serial or framebuffer console, booted standalone. Proves "runs on
      real hardware" — only half true today — and **every line counts for the server**.

      On the side something falls there that this project expressly keeps open: colour separation (A1)
      is measurable **on bare metal** as an effect, under KVM structurally not (§12).

### Z19. The SUBSTRATE of the language runtimes — what C/C++/Rust/Zig need before any service exists
**Class:** foundation · **Effort:** manageable and **completely enumerable** ·
**Delimitation:** without network stack, without file system — only what a runtime needs for *starting and computing*.

Looked up on 2026-08-09. The list is short, but four of its items stand **before** everything that is planned in
[Z16](#z16-translate-source-text-instead-of-running-binaries--assessed-2026-08-09) —
without them not even a `main(){return 0;}` starts.

- [ ] **A1. The process start stack is missing entirely.** A loaded program gets today
      `boot_arg` in **one register**. `_start` of musl, `lang_start` of Rust and Zig's `_start`
      on the other hand expect the **System V process start stack**: `argc`, `argv[]`, `envp[]` and —
      decisively — **`auxv`**.

      `auxv` is no formality: `AT_PHDR`/`AT_PHNUM` tell the runtime where its own
      program headers lie (the unwinder and the TLS set-up need that), `AT_PAGESZ` the
      page size, `AT_RANDOM` the bytes for the stack canary, `AT_HWCAP` the CPU features.
      Without `auxv` musl falls over at start, before one line of user code runs.

- [ ] **A2. There is no TLS base — and that hits C already without threads.** Neither `IA32_FS_BASE`/
      `wrfsbase` (x86) nor `TPIDR_EL0` (aarch64) is set for userland, and the trap frame
      does not carry it along; a context switch would thus not preserve it either.

      **`errno` is thread-local in musl.** Without TLS what is broken is thereby not "threading", but
      **the C library as such**. The same applies to Rust's `#[thread_local]` and C++'
      `thread_local`.

      To this belongs: **the loader does not know `PT_TLS`** (`elf.rs` delivers exclusively `PT_LOAD`).
      The TLS image of a program is thus not even found. Three parts: read `PT_TLS`,
      create the block per thread, set the base **and carry it across the context switch**.

- [ ] **A3. The stack is 16 KiB.** `LOADED_STACK_BYTES = 0x4000`. The usual default for the
      main thread is **8 MiB** (glibc, musl, Rust). Recursion, large stack frames in C++ or a
      formatter with a buffer on the stack run beyond it. That is no fine-tuning, but an order of magnitude
      — and it belongs in the manifest, not in a constant.

- [ ] **A4. SSE is not switched on on x86** — see [Z18](#z18-high-performance-for-translated-foreign-code--measured-2026-08-09) (1).
      `CR4.OSFXSR` is set nowhere, the SysV ABI demands XMM for `double`. Measured with the
      new `fp` check line.

- [ ] **B. Only afterwards does it become interesting — and these three are all that is missing without services:**

      | | for what | State |
      |---|---|---|
      | **Memory** (`mmap`/`brk`) | every `malloc`, every `Vec`, every `new` | missing (Z14 stage 1) |
      | **Console** (`write` on 1/2) | without it no program can report anything — no file system, a service | missing |
      | **Time** (`clock_gettime`) | Rust's `std::time`, C++ `<chrono>` | missing (vDSO page, W2) |

- [ ] **C. What comes additionally per language.**

      | Language | additionally | Remark |
      |---|---|---|
      | **Zig** | almost nothing | allocators are **explicit** (passed through), no hidden `malloc`. A Zig program with `FixedBufferAllocator` needs only A1–A4. **The cheapest first language.** |
      | **C** | A1–A4 + B | `errno` makes A2 mandatory |
      | **Rust** | + `std::sys` port | `panic = "abort"` already stands → **no unwinder needed**; in return `catch_unwind` drops out |
      | **C++** | + **unwinder** (`.eh_frame`, `libunwind`) | exceptions are the one real additional demand. Needs no syscalls, but `AT_PHDR` from A1 |

- [ ] **What "all possible programs" CANNOT mean, and that belongs written beside it.**
      Even with A and B and C not all run: `fork` without `exec`, `dlopen`, `mmap` of a file,
      `/proc`, and every program that writes `syscall` directly instead of via the libc. The honest
      formulation is **"everything that needs CPU, memory, console and time"** — that is very much
      (compilers, compression, cryptography, data structures, compute loads), but it is a
      nameable set and no universal statement.

---

## Z19 — the complete plan (2026-08-09)

**Principle that determines every individual decision below:** as little as possible in the kernel. For
every item it therefore stands what the kernel *must do* and what userland does itself.

### Step 0 — the console (brought forward)

Every acceptance **from step 2** is an output ("Zig reports `argc`", "two threads report
different TLS values", "computed result"). Without a console the first acceptance that
*can fail* could not be told from one that merely **cannot say anything** — exactly the
checker trap from the own log. The ring buffer (TCB-neutral, the kernel keeps its
own output) therefore belongs **before** step 2.

**Not before step 1:** its acceptance is the `fp` line, and that is printed by the **kernel**.
Step 1 is thereby self-supporting.

### Step 1 — A4: enable SSE (FOUR bits) + the eager/lazy decision

- **`[~]` Begun on 2026-08-09, NOT landed — see "State" below.**
- **Four bits, not one:**

  | Bit | for what |
  |---|---|
  | `CR0.EM` = **0** | as long as it is set, every FP instruction faults — **independent** of OSFXSR |
  | `CR0.MP` = **1** | lets `WAIT`/`FWAIT` trap correctly together with `TS` |
  | `CR4.OSFXSR` = 1 | enables `FXSAVE`/`FXRSTOR` **and** SSE |
  | `CR4.OSXMMEXCPT` = 1 | redirects SIMD FP exceptions to `#XM` instead of `#UD` |

- **On EVERY core** — `CR0`/`CR4` are core-local. One common function `hal::fp::enable_sse()`
  for BSP and AP, so that the two sites cannot drift apart, plus a counter
  "on how many cores enabled", which must be `num_cores()`.
- **Check `CPUID.1:EDX` beforehand** (FXSR bit 24, SSE bit 25) — otherwise `#GP` on foreign hardware.
- **The acceptance runs on an AP, not on the BSP.** A probe only on core 0 checks exactly the
  one site that one most likely does right.

- **The security decision belongs HERE: eager instead of lazy.**
  Lazy FP via `CR0.TS` is on Intel **LazyFP, CVE-2018-3665** — speculative execution reads the
  *old* FP register set before the `#NM` trap makes up the switch; the XMM registers of the previous
  thread become readable via a Spectre channel. Linux, Windows and the BSDs have therefore switched
  to eager.

  **Until today that was without consequence** — userland was soft-float, SSE was off, nothing stood in XMM. With
  `enable_sse()` real data stands there, and XMM is exactly the place where
  AES-NI key material lives. For a system whose selling point is isolation, lazy
  across PD boundaries would be no weighing between performance and security, but **between performance and
  its own thesis**.

  **The before-number already stands:** `Lazy-FP-Owner-Wechsel=399` from the aarch64 suite. Thereby
  the eager costs can be quantified, should somebody want to turn back — without it such a
  rollback would be an opinion.

- **`[x]` A4 LANDED (2026-08-09, third attempt) — and the way there was the yield.**

  **First two own false statements that belong corrected:**
  * "All four bits have the same effect on the fault line" was **architecturally false**
    and methodically unproved. `CR4.OSXMMEXCPT` shifts nothing from `#UD` to `#NM` — it
    decides only whether an *unmasked SIMD exception* is reported as `#XM`, and that
    presupposes running SSE code. Measured I had in addition three **combinations**, not four
    individual bits. A generalisation over measurements that did not exist.
  * The symptom I had read wrongly twice: it was a **hang** (time limit), and all 14
    `FAIL` lines were consequences of it.

  **The cheap measurement replaced the expensive analysis.** Red build under **TCG** with
  `-d int -D fault.log` (under KVM one sees nothing — the host core handles the interrupts):

      22: v=07 e=0000 cpl=3 IP=0023:...16a016   <- Ring 3 faultet auf FP (erwartet)
      23: v=07 e=0000 cpl=0 IP=0008:...12791b   <- der KERNEL faultet auf FP -- IM HANDLER

  Two lines, and the cause stands there. **No `#GP` on the CR write** — the write path
  is thereby exonerated, the alternative hypothesis refuted.

  **The cause:** `fp_trap` called `hal::fp::save`/`restore` **before** `set_el0_trap(false)`.
  `FXSAVE`/`FXRSTOR` are themselves FP instructions — with `CR0.TS` set they trigger exactly the
  trap from which they are called. Recursion, hang.

  **Why it was never noticed, and that is the actual lesson:** `CR0.TS` applies to **every**
  privilege level, `CPACR_EL1.FPEN` on ARM on the other hand only to EL0 — there the kernel may always
  compute, and **the same caller code is correct**. The x86 path was dead (`CR0.EM = 1` made SSE
  into `#UD`, x87 is not used by a soft-float kernel), so `#NM` never fired. **Enabling SSE
  makes dead code alive.**

  **Fixed in the HAL, not at the caller:** `save`/`restore` clear `CR0.TS` themselves. At the
  caller it would be discipline; in the HAL the next call site cannot forget it.

  **Landed:** `enable_sse()` (four bits, CPUID check, one function for BSP and AP),
  called on both — on the AP **after** `init_core()`, because before that no scheduler exists,
  and **before** the release, because `fxsave` saves XMM reliably only with `OSFXSR`. Counter
  `SSE_CORES` **without** `cfg` (a `cfg` only at the counter with an unconditional call site made
  `--no-default-features` no longer compile — that was the second of the two conflated errors).
  Acceptance: both configurations build, x86 `RUNS=5` green.

- [ ] **A4 remainder, and it follows from the eager decision:** in eager mode `#NM` is **no case
      to be handled any more, but by definition a kernel error** — the trap exists only
      because `TS` was set, and eager means: `TS` is never set. To *harden* the handler
      would thus be the wrong consequence; right is to replace it by a **loud message with RIP and core ID**
      and to take the invariant **"`TS == 0` after every switch"** into the check line.

      In doing so attention is due to the **lazy remainder**: the old machinery *sets* `TS` at the
      owner switch. If a path of it remains standing, eager is declared and lazy built — and the
      first FP-using user thread finds out.

      And: the aarch64 line `Lazy-FP-Owner-Wechsel=399` measures **something different** afterwards and must be
      renamed or redefined, otherwise the log compares before and after across
      a change of meaning.

- [ ] **The generalisation that is worth more than the individual case: a VECTOR INVENTORY.**
      "Handler installed, never executed" is a **class**, and it has now been hit twice
      (the missing x86 `fp` check line was the same gap from the other side).
      A reporter for the first execution is thought too small: **every trap vector gets a
      counter**, and the suite prints at the end which vectors on which architecture have ever fired.
      An array and a print line — and from "dead code that nobody knows" becomes an
      inventory list that comes along with every run. A path that according to the inventory fires on aarch64
      and never on x86 is then no chance find when debugging, but an open line in the
      log.

- [ ] **Not yet back in** (fell away on resetting, reconstructible from the description):
      the ring-3 `fp` probe with two different patterns, its run on an
      **AP** instead of the BSP, and the `fp` line in `all_done()`. They come together with the vector inventory,
      whose witness it is.

### Step 2 — A1: the process start stack

- **A find that had to be clarified beforehand:** the program headers lie in **no** `PT_LOAD`.
  `programs/user-x86.ld` begins with `. = 0x20000000;` without `SIZEOF_HEADERS`; `readelf` confirms
  it (first `LOAD` at offset `0x1000`). **`AT_PHDR` thereby pointed into the void** — and on that hung
  both the TLS set-up and the C++ unwinder. Fix in the linker script:
  `. = 0x20000000 + SIZEOF_HEADERS;`, so that the first segment maps from offset 0. Standard practice,
  costs nothing, but must happen **before** A1.
- **Where:** `load_into_pd_mit` (`system.rs`), immediately before `spawn_user_at_parked`; and
  `init_thread_frame` (`crates/caprock-hal/src/x86_64/exception.rs:338`) gets a finished
  `rsp` instead of an argument in `rdi`.
- **Layout, from top to bottom:** strings (`argv[0]`, environment) · padding ·
  `auxv` (terminated with `AT_NULL`) · `envp` (NULL) · `argv` (NULL) · `argc`. `rsp` points to
  `argc`.
- **`auxv` minimum set:** `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_ENTRY`, `AT_RANDOM`
  (16 bytes), `AT_HWCAP`, `AT_NULL`.
- **Trap, and it is already half documented in the source text:** SysV demands at **function**
  entry `rsp % 16 == 8` (as if a `call` had just pushed the return address) — at **process**
  entry on the other hand `rsp % 16 == 0`. The comment at `init_thread_frame` describes
  today the first case; for `_start` the second applies. Whoever mixes these up gets a
  `#GP` at the first 16-byte-aligned SSE operation — i.e. **only after step 1**, and
  then it looks like an FP error.
- **`AT_RANDOM` needs real randomness, and that does not exist at load yet.** `virtio-rng` is
  a driver PD and runs later. A TSC mix is **no** randomness; whoever uses it must
  write that down, otherwise the stack canary is ornament. Honest interim solution: fill `AT_RANDOM` with
  a named source marked as weak and keep the item open.
- **Acceptance:** a Zig program reads `argc` and `AT_PAGESZ` from its start stack and reports the
  **values**. Not "started" — a program that starts and reads nonsense would look the same.

### Step 3 — A2: TLS

- **The insight that halves the effort:** musl sets up TLS **itself**. `__init_tls`
  finds `PT_TLS` via `AT_PHDR`, creates the block and then calls `arch_prctl(ARCH_SET_FS)`. The
  kernel thus need **not understand** `PT_TLS` — the loader remains unchanged. It needs three
  things:
  1. `AT_PHDR` (comes from step 2),
  2. **a syscall "set the TLS base of this thread"**,
  3. the base **in the TCB** and carried across the context switch.
- **Kernel part:** x86 write `MSR_FS_BASE` (`0xC000_0100`) — or `CR4.FSGSBASE` + `wrfsbase`,
  which is faster and constitutes an item of its own. aarch64: `TPIDR_EL0`. In addition a field in the TCB
  and a write access in the switch.
- **Security note that belongs to it:** the TLS base is an arbitrary user address. The kernel
  **never dereferences it** — `%fs:0` is read exclusively by ring 3. Otherwise the syscall would be a
  "read me an address of my choice" service.
- **The model-fidelity guard will complain about this** (new TCB field, new write site). That is
  expected and belongs in the register — see `admitted` on 2026-08-07.
- **Acceptance:** two threads with **different** TLS values, checked mutually over N switches
  — the same form as the `fp` probe. Different is again the point: with equal values a missing
  save would not be noticed.

### Step 4 — A3: stack and guard page

- **Do not guess, measure.** How deep does a Zig/C hello really go? Fill the stack with a pattern,
  count the watermark after the run. Only afterwards fix a number.
- **Interim:** `LOADED_STACK_BYTES` (today 16 KiB) to the measured need plus reserve.
- **Right:** into the manifest — and that needs more than `entry_len = 96`, i.e. **format version 2**.
  This version can take `period_us` from [Z16](#z16-translate-source-text-instead-of-running-binaries--assessed-2026-08-09)
  along: **one** version for two fields instead of two versions.
- **A guard page belongs to it, otherwise "larger stack" is only "broken later".** Without it
  an overflow silently overwrites what lies beneath.
  **Acceptance:** a recursion of defined depth runs; a deeper one **faults cleanly** — and the
  fault lies in the guard page, not somewhere.

### Step 5 — B: the three services

| | Design | Remark |
|---|---|---|
| **Memory** | Z14 stage 1: a PD holds a large memory cap and hands out derived ones via IPC REPLY | TCB-neutral, `CCOPY` and cap transfer both already stand |
| **Time** | vDSO page (W2): read-only in every PD, tick counter + frequency | no syscall; exactly what Linux itself does |
| **Console** | see below — **a design decision is to be made here, no implementation** | |

**The console question, written out:** the kernel owns today the serial interface for
its own reports. Three ways:

* **(a) Debug syscall.** Cheap, but the TCB grows, and a "for debugging only" syscall remains.
* **(b) Console PD gets the device.** Clean — but the kernel would lose its output, and that
  is the carrier of every check line. Not acceptable for the self-test.
* **(c) Shared ring buffer.** The program writes into a page, the kernel empties it at
  the report. **TCB-neutral**, and it is the same form as Z18(5).

**Recommendation: (c).** If (a) is taken as an interim step, it belongs marked as *debug*
and expressly named as non-productive in `docs/invariants.md` — otherwise it stays.

### The acceptance chain, and why it separates in this order

1. after **A4**: `fp : ALL PASS`, in `all_done()` — SSE **and** lazy save proved.
2. after **A1**: Zig reports `argc` and `AT_PAGESZ` — the process start carries, **without** a memory service.
3. after **A2**: two threads, different TLS values, preserved over switches — `errno` would be possible.
4. after **A3**: defined recursion runs, deeper faults in the guard page.
5. after **B**: a **Zig** program with `FixedBufferAllocator` computes and reports a
   **computed** result via the console.

**Zig first, and that is no taste:** it is the only one of the four languages that gets by without a
memory service (allocators are passed through explicitly). Thereby the acceptance separates the
**process start** from the **memory question**, instead of checking both at once — and if step 5
fails, it is known that 1–4 stand.

### What this plan does NOT solve, and that belongs to it

* **C++ exceptions** need the unwinder (`.eh_frame` + `AT_PHDR`). A1 delivers the precondition,
  nothing more.
* **Rust's `std`** additionally needs the `sys` port; `panic = "abort"` already stands, an unwinder
  is thus not needed there.
* **Threads** do not occur. A program that *uses* `std::thread` does not run according to this plan
  — only one that at most links them.
* **`fork`, `dlopen`, `mmap` of a file, `/proc`** remain outside, see Z19 delimitation.

**Effort estimate, expressly marked as an estimate:** A4 is hours, A1 and A2 days each,
A3 hours plus the format version, B (time) hours, B (memory) and B (console) days each. The numbers
are guessed — what would make them reliable would be the first implementation, and that is why A4 stands first.

### Z18. High performance for translated foreign code — measured 2026-08-09
**Class:** performance · **Effort:** one item is almost free, one is a precondition for everything
further · **Boundary condition:** isolation and small TCB remain.

Compatibility ([Z16](#z16-translate-source-text-instead-of-running-binaries--assessed-2026-08-09))
and **performance** are different axes, and on the second microkernels historically lose.
The reasons are however nameable and individually measurable.

- [ ] **(1) The user target computes floating point in SOFTWARE — measured, not conjectured.**
      The same function (dot product over 64 `f64`), once for an ordinary x86_64 target and
      once for `programs/x86_64-caprock-user.json` compiled:

      | | Instructions | SSE FP | Library calls |
      |---|---|---|---|
      | ordinary x86_64 target | 18 | **8** (`mulsd`/`addsd`) | none |
      | **project target** | 53 | **0** | **4× `__muldf3` + 4× `__adddf3`** |

      Every multiplication and every addition becomes a **call**. A `mulsd` costs ~4 cycles;
      `__muldf3` unpacks, multiplies 64×64→128 bit, normalises, rounds, packs — order of magnitude
      50–150 cycles with call costs. For numerical code that is the difference between
      usable and unusable.

      **And the kernel does not demand it.** It has complete lazy FP: `fxsave64`/`fxrstor64` with a
      `CR0.TS` trap on x86, `CPACR_EL1.FPEN` on aarch64 — exactly so that user threads *can* use FP.
      The `+soft-float` in the **user** target has migrated along from the decision "soft-float
      microkernel" (ext-3/ext-5); for the kernel it is right, for userland not.

      **CORRECTION (2026-08-09, after objection): this is no performance item, but a
      Z16 BLOCKER on x86.** The x86-64 SysV ABI *presupposes SSE2* — `double` is passed and
      returned in XMM registers. Re-measured on the same function `f64 -> f64`:

      | | Arguments | Return |
      |---|---|---|
      | ordinary x86_64 (SysV) | `%xmm0`, `%xmm1` | `%xmm0` |
      | **project target** | `%rdi`, `%rsi` | `%rax` |

      These are **two calling conventions**. A musl built upstream assumes the first; the
      linker sees only equal symbol names and cannot notice the mix-up — soft-float
      caller against hard-float library is **silent corruption**, no error message. The
      alternative would be a custom-ABI musl, i.e. exactly the fork maintenance that was rejected at
      [Z17](#z17-turso-as-a-native-database--pre-measured-2026-08-09-not-begun). The decision thus stands fixed **before** the musl port, independent of any
      cycle count.

      **Two conditions before this is switched over:**
      * **[x] The x86 `fp` check line has stood since 2026-08-09 — and it found something at once.**
        Two ring-3 probes load different patterns into `xmm0..xmm3`, yield via `YIELD` and
        check after every return. Result: `gelaufen=true`, patterns preserved **`0b0`** of
        `0b11`.

        **Cause measured, not conjectured: `CR4.OSFXSR` is set nowhere.** Without this bit
        *every* SSE instruction triggers a **`#UD`** instead of a `#NM` — the probe faults at the
        first `movq xmm0` and never reaches the owner switch. The `#NM` handler exists
        (`exception.rs:437`), `fxsave64`/`fxrstor64` too; what is missing is the **enabling of the
        instruction set**.

        Thereby it is also clear that `+soft-float` in the user target was no remnant, but
        fits a kernel that never switched SSE on. The work is thus: set `CR4.OSFXSR` (+
        `OSXMMEXCPT`), get the check line green, **then** switch the target.

        The line does **not** stand in `all_done()`. That is a decision: to colour a known gap
        permanently red conceals every future regression. It will be gated as soon as
        `CR4.OSFXSR` is set — before that it would be a demand that this configuration
        *cannot* fulfil, the same form as `root` without an archive.

      * ~~On x86 there is no `fp` check line~~ — only aarch64 measured the lazy FP path
        (`fp : EL0-FP-Threads-OK=0b11/0b11, Owner-Wechsel=399`). To switch SSE on would mean arming
        a path that was **never executed** on this architecture. The same error form
        as "the colour test lay in the x86 boot-up and never ran on aarch64", only
        mirrored. First the check line, then the target.
      * **`fxsave` covers x87/MMX/SSE, not AVX.** SSE/SSE2 are permitted with the existing saver;
        to permit AVX without switching to `xsave` would be a silent
        register loss between two threads — the worst sort of error.

- [ ] **(2) The IPC costs are NOT MEASURED — and that is the actual gap.**
      In the whole tree there is no cycle measurement of an IPC round trip. On a microkernel that is
      the determining quantity: every service call (read file, fetch memory, wake futex) is a
      round trip instead of a syscall. **"High performance" without this number is a wish, no
      statement** — and without it it is also not decidable whether items (3) and (5) pay off.

      The infrastructure stands: `crates/caprock-sched/src/cycles.rs` (B-5.1), `hal::timer::cycles()`
      with `rdtscp`+`lfence`, and `cpuid` is already known as a trap (3556 instead of 51 cycles under
      KVM).

      **To make the measurement DECISION-CAPABLE it needs three separations and two anchors** —
      a bare round-trip value carries little:

      | Separation | why |
      |---|---|
      | fastpath against slowpath | if the IPC path has the distinction at all; if it does not, **that** is the finding |
      | with against without address-space switch | ping the same PD via notification against round trip between two PDs |
      | warm against cold | the hot measuring loop measures the best case; a round trip after artificial TLB pollution measures the one a service sees under load — **and that is at the same time the before-number for (3)**, otherwise the PCID gain cannot be quantified later |

      **The two anchors without which the number is meaningless:** seL4 publishes fastpath IPC in
      the region of a few hundred cycles on x86; a Linux syscall lies with mitigations at roughly
      100–200. **Below 1000** would mean: the service architecture carries. **Several thousand** would mean:
      (4)/(5) are no optimisations, but preconditions.

      **Acceptance:** a line `ipccost` with median and scatter per case, plus a
      positive control that shows that the measurement also sees an artificially slowed path.

- [ ] **(3) Every address-space switch empties the WHOLE TLB — and PCID is NOT a bit that one sets.**
      Without `invpcid`, on unmapping one must invalidate across *all* PCIDs or keep a generation counter.
      **Stale TLB entries across PCID boundaries are the most silent class of error that
      a kernel can have:** a cap withdrawal whose mapping survives in the TLB of a foreign PCID
      would be a **security hole**, no performance error.

      On aarch64 the counterpart on the other hand comes almost as a gift (ASID in the `TTBR`, `tlbi aside1is`).
      **Cheap order, if (2) shows that the TLB loss dominates:** arm ASIDs on aarch64
      first and measure the gain there, before the PCID machinery arises on x86.

      On the actual state: the ASID is today "a pure **software** number of the kernel" (`mmu.rs`),
      PCID is not used; with IPC-heavy loads the address space switches twice per round trip.
      Stands as an optimisation already in [C3](#c3-cap--pd--ipc-tabellen-dynamisch) — with (2) it becomes
      measurable instead of plausible.

- [ ] **(4) Do not copy data through IPC, but share memory.** The pattern already stands:
      the fs PD reads via a **shared transfer surface**, not via message words. The
      next step is to map file pages directly into the client PD (`mmap` of a file) —
      then a read access costs **zero** IPC instead of one per buffer.

- [ ] **(5) Batch instead of asking individually.** A ring of requests and completion messages
      between two PDs (io_uring form) turns N round trips into one. That is exactly the form that
      [Z17](#z17-turso-as-a-native-database--pre-measured-2026-08-09-not-begun) expects anyway
      — Turso's I/O traits are built around it.

- [ ] **(6) What is already there and merely belongs named.** Budget donation (ADR 0019, `sc_donor`/
      `sc_donee`): the server computes on the client's account, instead of needing its own time —
      that is the classic answer to "the service is not scheduled while the client
      waits". The IPC fastpath (`switch_to`) also exists.

- [ ] **Order.** (2) first — without the number everything further is guessed. Then (1), because it has
      the largest measured individual effect and costs almost nothing as soon as the x86 `fp` check line
      stands. Afterwards the measurement decides whether (3), (4) or (5) pays off first.

### Z17. Turso as a NATIVE database — pre-measured 2026-08-09, not begun
**Class:** application · **Effort:** small for the core of the matter, large for the surroundings ·
**Order: AFTER the Rust `std` port from [Z16](#z16-translate-source-text-instead-of-running-binaries--assessed-2026-08-09)**

- [ ] **First the role question, because a wrong decision hung on it.** `sqlite3` stands in Z16 as a
      **measuring instrument**, not as payload: the acceptance "sqlite3 passes its test suite" checks
      not whether there is a database, but whether the POSIX layer does `fcntl` locks, `fdatasync`,
      `pread64`/`pwrite64` and the error paths right — under a load that a foreign
      team has hardened over 25 years.

      **To replace it by a Rust reimplementation would be circular:** the platform would be checked with
      software that runs through the **own** Rust `std` port — i.e. over a substructure
      that this project itself has written. Over the musl layer that the milestone measures, that
      would prove nothing. In addition the dependency inversion: Turso needs the Rust `std` port, and
      that comes in Z16 **after** the sqlite3 milestone — the acceptance of stage 5 would stand on
      stage 7.

      **Z16 therefore remains unchanged.** `sqlite3` in C via musl.

- [ ] **And a fork would be the worst of the variants.** An own branch of a beta project
      with a full-time team is a permanent rebase against a moving target. Either from scratch
      or upstream unchanged — but no maintained divergence.

- [ ] **Where it is really good: as a native backend that BYPASSES the POSIX layer.** Turso
      abstracts its I/O behind traits; io_uring is **one** backend, not the assumption. A
      Caprock backend would map these traits directly onto IPC and caps — no `fcntl`, no vDSO,
      no VFS detour — and would be an **upstream contribution instead of a fork**. It would be at the same time the
      first application that shows the actual advantage of this architecture: native software
      does **not need** the POSIX environment, only the compatibility layer gets it.

- [ ] **The pre-measurement, before the first line** (2026-08-09, `tursodatabase/turso`, shallow clone):

      | | |
      |---|---|
      | Existing backends | **7** (`io_uring`, `unix`, `windows`, `win_iocp`, `generic`, `memory`, `vfs`) — the abstraction demonstrably carries |
      | `trait File` | 17 methods, of which **7 without default** |
      | `trait IO` | 15 methods, of which **2 without default** |
      | Mandatory surface in total | **9 methods** |
      | Smallest complete backend (`generic.rs`) | **117 lines** |
      | `supports_shared_wal_coordination` | default **`false`** — the whole `shared_wal_*` family is voluntary |

      **And the mapping onto what Caprock already has:**

      | Method | Mapping | State |
      |---|---|---|
      | `pread` / `pwrite` | block service `OP_READ`/`OP_WRITE` (sector-wise, position-carrying) | **stands** (A-6.1) |
      | `sync` | `OP_FLUSH` | **stands** |
      | `size` | `OP_INFO` resp. the file size of the fs PD | **stands** |
      | `open_file` | fs PD | half (A-6.3 reads one file; no paths) |
      | `truncate`, `remove_file` | fs PD, FAT level | missing, ordinary work |
      | `current_time_*` | the vDSO page from **W2** | missing, cheap |
      | `lock_file` / `unlock_file` | **here the cap model is BETTER than POSIX** | see below |

      Four of the nine thus map directly onto the existing block service.

- [ ] **The interesting point is the locks, and it turns out in our favour.**
      `generic.rs` makes `lock_file` a **no-op** (`Ok(())`) — the backend assumes it is the
      only writer. POSIX `fcntl` locks are advisory, process-wide and notoriously
      brittle; on a cap system "exclusive access to this file" on the other hand is exactly
      what a **cap** expresses: whoever has it, has it, and there is no second way.

      That is the sentence that makes this strand worthwhile — not "runs too", but "is stricter here
      than on Linux". It is however **unproved** as long as the backend does not exist,
      and therefore does not belong in `docs/invariants.md` before it is measured.

- [ ] **Acceptance, when the time comes: not "runs", but "passes Turso's DST suite"** —
      deterministic simulation and fuzzing. With the caveat that belongs to it: that is a
      **compatibility test against SQLite**, no independent correctness corpus of the
      order of magnitude of TCL plus TH3. As a yardstick for *the backend* it suffices; as a yardstick for
      the file semantics of a platform it would be the weaker instrument — exactly for that reason
      `sqlite3` stays standing in Z16.

- [ ] **Caveat on maturity, second hand and not measured myself:** the maintainers
      describe libSQL as production-ready and the Rust engine as beta. Before a product step
      that would have to be re-checked; for a backend contribution it is immaterial.

### Z16. Translate source text instead of running binaries — assessed 2026-08-09
**Class:** product strand · **Effort:** large, but **an order of magnitude smaller than
binary compatibility** · **Boundary condition:** isolation and small TCB remain.

> **ATTENTION, 2026-08-10: the premise of this entry no longer applies alone.** The sentence further
> below — "binary compatibility falls away, thereby the syscall redirection falls away" — has been
> **reversed** by [Z27](#z27-binary-compatibility-is-from-now-on-a-line--the-reversal-of-z16-decided-2026-08-10):
> the syscall redirection is built (Z26/A3). Z16 is thereby **one of two
> lines**, not the line. What stands here remains right for everything that exists as source text; for blobs (`libcuda`) Z27 applies. Whoever reads only this entry reads half the
> decision.

**The goal (Simon, 2026-08-09):** clone source text, translate, run on the microkernel —
as many Linux libraries as possible from C, C++, Zig, Rust, Go.

- [ ] **What this goal STRIKES, and that is the actual yield.** Binary compatibility falls
      away. Thereby all four kernel operations from [Z14](#z14-foreign-software-without-a-guest-layer--assessed-2026-08-09) fall away:
      no syscall redirection, no `ld.so` for foreign binaries, no ABI fidelity towards Linux,
      no personality server. What remains is **one libc and the services behind it** — and
      they may have their own ABI, because everything is recompiled.

      That is also the difference from WSL1/gVisor, on which binary compatibility failed:
      there Linux **behaviour** had to be replicated. Here only POSIX **semantics** has to
      be right, and where a library goes beyond that, the source text is there.

- [ ] **Measured on 2026-08-09, `strace -f` over five real programs:**

      | Program | distinct syscalls |
      |---|---|
      | C hello, statically linked | **14** |
      | `git --version` | 22 |
      | `python3 -c pass` | 34 |
      | `sqlite3` | 36 |
      | `git clone` (local) | 38 |
      | **Union** | **48** |

      Side finding that confirms W2: **`clock_gettime` does not appear** — glibc fetches the time from
      the **vDSO**. The read-only mapped page with tick counter is thus not a makeshift, but
      exactly what Linux itself does.

- [ ] **The work is not the libc, but what stands behind it.** The union, sorted by
      services — and beside it what Caprock has of it today:

      | Service | in the measurement set | State today |
      |---|---|---|
      | memory (`mmap`/`brk`/`mprotect`) | 4 | **missing** — Z14 stage 1 (memory server), TCB-neutral |
      | files (`openat`/`read`/`getdents64`/…) | 10 | half: fs PD reads/writes FAT16 **one file**; no paths, no `/dev`, no `/proc` |
      | threads (`futex`, `set_tid_address`, …) | 5 | **missing** — one thread per PD; needs a kernel operation |
      | processes (`execve`, `clone`, `wait4`) | 5 | half: `SYS_LOAD` creates PDs; no `fork` |
      | signals | 3 | **missing** |
      | time | (vDSO) | **missing** — see W2, cheap |
      | network | 0 in the measurement set, indispensable for a cloud | virtio-net driver stands, **no TCP/IP** |

      **Exactly one of these lines needs the kernel** (threads in a PD). All others are
      services in PDs — the TCB stays at 236 KiB.

- [ ] **Per language, and the order follows from it.**

      | Language | Way | Difficulty |
      |---|---|---|
      | **C** | port musl (~90,000 lines, only the syscall layer is ported) | **foundation for everything further** |
      | **Rust** | `std` port (one `sys` layer, as Redox/Hermit/UEFI have it) | medium — and it is the language of this project |
      | **Zig** | brings musl itself; `std.os` needs a layer | presumably cheapest after C |
      | **C++** | libc++ over musl, plus unwinding for exceptions | medium, hangs on C |
      | **Go** | own runtime, **bypasses libc on principle**, needs threads, signals (preemption), `mmap` | **own strand**, most expensive line — realistically last or via WASI |

- [ ] **The precedent, and it gives courage instead of fear.** **Redox OS** is the same form: a
      microkernel in Rust, an own libc (`relibc`), and on it ported software in quantity — with
      a small team. That is the reliable reference for this way, not gVisor or WSL1;
      they failed on a requirement that this goal does not pose at all.

- [ ] **The trap that this strand brings along: "translated" is not "working".** A
      library that builds through can be wrong in every second call. The acceptance is
      **its own test suite**, not the compiler — and for every ported library the
      number belongs in the log (`sqlite3` has about 700 tests, `zlib` a handful, `openssl` thousands).
      Otherwise the same sort of green arises as with a checker that cannot fail.

- [ ] **An assumption that belongs stated: translation happens on a Linux machine
      (cross), not on the kernel.** Self-hosting ("clone and compile **on** Caprock")
      is a different order of magnitude: it needs `fork`/`exec`, a full file system, much
      memory and a compiler as a ported application. That is the freestyle, not the entry —
      and if it is the goal after all, it changes the order below.

- [ ] **Measured on 2026-08-09 on TEST LOADS instead of start calls** — and that shifts the
      priorities. `strace -f` over zlib (`example`), sqlite3 (real disk, WAL, 20,000 rows,
      `VACUUM`, `integrity_check`), a 4-thread lock test and Git:

      | Load | Syscalls | futex | fork/`clone` | signal **delivery** |
      |---|---|---|---|---|
      | zlib `example` | 19 | — | — | — |
      | sqlite3 (disk, WAL, VACUUM) | 33 | — | — | — |
      | `git status` | — | 1 | — | — |
      | 4 threads, 800,000 lock cycles | — | **5,863** | 4× `clone3` | — |
      | `git clone` | 38 | 903 | 29× `clone3`, 4× `wait4`, 6× `execve` | — |
      | `python3 -c pass` | 34 | 1 | — | — (66× `rt_sigaction`, i.e. only INSTALLING) |

      **Three findings that change the plan:**

      1. **The heavy part is the file semantics, not the threads.** sqlite alone demands
         `fcntl` (locks), `fdatasync`, `pread64`/`pwrite64`, `ftruncate`, `mremap`, `getcwd`,
         `unlink` — and needs **neither futex nor fork nor signal delivery** for it.
      2. **Signal DELIVERY occurs in no measured load.** Only `rt_sigaction`/
         `rt_sigprocmask`, i.e. the entering of handlers. `rt_sigreturn` appeared exclusively
         via the `sh` shell, not from the programs.
      3. **0.7 %.** 800,000 lock cycles yielded 5,863 futex calls — the uncontended path
         never enters the kernel. What a userspace futex costs thus concerns 0.7 % of the
         lock operations, not all.

- [ ] **THREE decisions that belong BEFORE the musl port — not as a surprise in a
      later milestone.** (And a correction: "exactly one line needs the kernel" was too
      convenient. It is **one for sure**, a **second conditionally**, and a third is avoidable —
      but that is a decision, no fact.)

      **(1) futex: service over the EXISTING blocking primitive, no new syscall.**
      The hard question is "what does the caller block on?", and it has here an answer that
      costs nothing: **on its own notification, via the existing `SYS_WAIT`.** The
      kernel already has the blocking primitive; the service only keeps the wait queue and
      signals specifically. `Notification::wait` holds exactly **one** waiter (since D11
      expressly, with `ERR_EP_FULL` instead of silent overwriting) — that fits if each thread
      has its own.

      **Costs, measured instead of estimated:** 2 IPC round trips per *contended* wait instead of one
      syscall — on 0.7 % of the lock operations. **TCB growth: zero.**

      **What would overturn this decision** (and that belongs to it, otherwise it is irrefutable):
      a load in which the contention rate is so high that the round trip dominates. **And the
      known alternative belongs named along, so that the decision does not stand without alternative:**
      with a request ring ([Z18](#z18-high-performance-for-translated-foreign-code--measured-2026-08-09) (5))
      plus notification the waking becomes an **entry** instead of a round trip — the calculation "2 round trips
      per contended wait" then no longer applies, and a kernel futex would not be
      the only answer even then. The measurement
      for it stands fixed: the same lock loop, rate of the futex calls per lock operation, and the
      wall time against a version with a kernel futex. Only then is a kernel futex a finding
      and no preference.

      **(2) Signals: synchronous first, asynchronous is a KERNEL upcall.**
      Stage A — handlers are kept in the libc and delivered at **syscall boundaries**
      (`EINTR`, `SIGPIPE` as `EPIPE`, `SIGCHLD` at the `wait`). That covers **every measured load**
      and costs the kernel nothing.
      Stage B — real asynchronous delivery (SIGALRM into a compute loop, SIGINT from the terminal)
      demands interrupting a running thread and building a handler frame for it.
      **No service can do that via IPC**, that is an upcall and thereby a TCB change. It is
      built when a test suite demonstrably demands it — and the line in the log is then
      `rt_sigreturn` from the program itself, not from a shell.

      **(3) fork: `posix_spawn` first, real `fork()` only on proof.**
      Measured, `git clone` forks (29× `clone3`, 6× `execve`, 4× `wait4`) — but as
      **fork+exec**, and exactly that is covered by `posix_spawn`. It maps cleanly onto the existing
      way: create PD, load, start (`SYS_LOAD`). A real COW `fork()` would need
      fault redirection and mapping into a foreign PD — both TCB — and is built only when a
      test suite demands it (a `fork` **without** a following `exec` that uses the child state).

- [ ] **Order, each stage with an acceptance of its own.**
      1. **Memory server** (Z14 stage 1) — without `mmap`/`brk` no libc runs. TCB-neutral.
      2. **Clock as vDSO page** (W2) — cheap, and without it almost every library is unusable.
      3. **musl port**, first against a target without files: `write`/`exit`/`brk`/`mmap`.
         **Acceptance:** a C hello, built from source text, runs — and its 14 syscalls are
         individually proved, not merely "it ran".
      4. **fd table + VFS service** over the existing fs PD; paths, `getdents64`.
         **Acceptance:** `zlib` builds and passes its test suite (19 syscalls, measured).
      5. **The actual chunk: file semantics.** `fcntl` locks, `fdatasync`,
         `pread64`/`pwrite64`, `ftruncate`, `unlink`, `getcwd`. **TCB-neutral**, but it is the
         largest single stage — measured at sqlite, which needs exactly that and nothing else.
         **Acceptance:** `sqlite3` passes its test suite **without** threads, without fork, without signals.
      6. **Threads** (the one kernel operation) + `futex` as a service over notifications.
         **Acceptance:** the 4-thread lock test delivers 800,000 as the result, and the measured
         rate of blockings stands in the log.
      7. **Rust `std` port**, afterwards C++/Zig.
      8. **TCP/IP service** over the existing virtio-net driver — from here on it is a cloud.
      9. Go: own strand, own decision.

### Z15. WASM in a PD — the plan (2026-08-09)
**Class:** new subsystem · **Effort:** staged, W1 is small · **TCB effect:** zero up to W2

All numbers from [Z14](#z14-foreign-software-without-a-guest-layer--assessed-2026-08-09). Every stage has
an acceptance that **can fail**; a stage without a counter-proof does not count as finished.

- [~] **W1 — begun 2026-08-09, state: the PD runs, the REPORT PATH does not.**

      **Proved (not conjectured):** `programs/userland/wasmhost` builds against `wasmi 0.31` for
      `x86_64-caprock-user` (429 KB ELF, three PT_LOAD: 257 KB code, 30 KB rodata, **2 MiB pure
      BSS** as heap arena). The loader accepts it — the BSS segment with `FileSiz 0` is exactly the
      case that `copy_segment_at` has handled since the colour rework. **The PD runs**, measured with
      a cap-free probe (a fault at `0xDEAD_0000` appears in the log), both with
      64 KiB and with 2 MiB arena.

      **Open:** none of the four badges arrives. The report path is `ccopy` (own badged copy)
      + `signal`, the same pattern as `init` at A-3.1.

      Excluded by looking in the handler, not by trying:
      * **Rights amplification** — `ccopy` *intersects* (`rights_from_bits(mask).intersect(have)`),
        a too-large mask is thus harmless. (My first "correction" from `7` to `0b010` rested on the
        wrong assumption and was itself an error.)
      * **Slot range** — `NCAPS = 16`, the target slots 6..9 are valid.
      * **Occupied target slot** — the loader endows into 0..5.
      * **Load error / segment size** — see above, the probe runs.

      **The next lead, and it is concrete:** the handler hands out a copy cap **rejected** at
      `install_cap_checked`, in order to delete it — the domain policy thus judges the copy.
      `init` is TrustedSas, `wasmhost` is UserLand. To be checked is whether a UserLand PD may install
      a self-minted notification copy at all. Second lead: which notification **object** the kernel reads —
      `CLIENT_NTFN` is overwritten by every non-root, non-driver PD, and in this start set those are
      `hello`, `fs` and `wasmhost`.

      **What this search has already taught:** I twice changed two things at once (heap size and report path;
      rights and source slot) and thereby made two runs worthless for myself.
      The cap-free probe was the first step that decided anything — because it asks **one**
      question and needs no cap.

- [ ] **W1 (remainder) — the engine runs in a PD, without a single new kernel line.** A new program
      `programs/userland/wasmhost` links `wasmi` against `libcaprock`, takes a `.wasm` from the
      boot archive (a third way beside ELF and manifest), instantiates it and reports the result
      via its endpoint.

      **The heap is a bump allocator over the private region** — that is exactly the model that
      a PD has today (fixed memory, no `brk`), and it is measured as sufficient.

      **Acceptance:** `wasm : ALL PASS` with the *computed* result of the guest module, not with
      "ran through". In addition two negative cases, because otherwise the line proves only that an engine
      starts: a **mutated** module (one byte in the code section) must be rejected, and
      a module that reaches beyond its linear memory must trigger a WASM trap —
      **without** the PD faulting. The second case is the actual statement: the sandbox holds
      *inside* the PD, and the PD isolation is the second line.

- [ ] **W2 — the WASI core, and a blocker that is small.** `proc_exit`, `fd_write` (console),
      `random_get`, `clock_time_get`. The first three go via existing services resp. caps.

      **`clock_time_get` has no basis today: there is no time ABI** (0 hits for
      `CLOCK`/`GETTIME`). The cheapest way, which almost does not touch the TCB, is **no syscall**,
      but a **read-only page mapped into every PD with tick counter and frequency** — the
      kernel writes it anyway, the reader needs no transition. That is the vDSO idea,
      and it costs a mapping plus a write access in the timer path.

      **Acceptance:** the clock must be *monotonic* and *plausible* — two readings with a
      known wait in between, and the difference lies in the expected band. A counter that
      stands cannot otherwise be told from one that runs.

- [ ] **W3 — memory server (Z14 stage 1) → `memory.grow` and more than one guest.** Only here
      does the matter become multi-tenant-capable. Acceptance as described in Z14, plus: two WASM PDs
      at the same time, and the one does **not** see the linear memory of the other (positive control
      over the same server, only one address moves — the same form as A-5.4).

- [ ] **W4 — files via the fs PD** (A-6.3): `path_open`, `fd_read`, `fd_seek`, `fd_close`.
      The PD drives no device, it calls the block service — the way has stood since A-6.3. Acceptance: a
      guest reads a file whose content an **independent** reader (`tools/checkfat.py`)
      confirms.

- [ ] **W5 — one guest per PD, and it stays that way.** Several guests in one engine would be cheaper and
      would be the abandoning of the isolation statement: the separation of two tenants would then lie
      in the engine instead of in the kernel. That is exactly the layer that this project does not want to have.
      **Belongs as a stipulation in `docs/invariants.md`**, not in a comment.

- [ ] **What is NOT planned and why.** A JIT (Cranelift): it needs executable memory written to
      at run time — i.e. softening W^X or a `mprotect`-like
      operation. Both are expensive at the place where this project wants to yield least.
      The interpreter costs a factor of 5–20 in computing time; that is the price, and it is
      measurable instead of claimed.

### Z13. The block service protocol stands THREE times (measured 2026-08-07)
**Class:** drift · **Effort:** small

Measured during the investigation for Z12:

| Copy | Opcodes | Status codes |
|---|---|---|
| `programs/hardware/virtio-blk` (server) | 6 | 5 |
| `programs/trusted/fs` (client) | 5 | 1 |
| `kernel/src/arch/x86_64/bringup.rs` (client) | 7 | 0 |

Three origins for **one** statement, held solely by discipline — and they **already
deviate**: `OP_STOP` is known only to the server, the status codes are almost unknown to the clients (`fs`
checks only against `ST_OK`, the kernel against none). That is no error today, because the numbers
agree; nothing enforces it.

The same with the **layout of the transfer surface**: `OFF_ERGEBNIS = 4096` (fs), `OFF_SERVED = 0x600`
(server), `OFF_DATA` from `caprock_virtio::blk` — the service protocol borrows its layout from
the driver crate.

**That is the cheaper and closer cut than Z12**, because it is our own format:
one source, three products, and the existing `iface_version` gate from A-4.4 could take its
version **from the hash of the description** — then the wire format cannot change
without a hot reload being rejected. No new mechanism, only a coupling that is
missing today.

## Z5. Tickless for compute cores
**Class:** scheduler/timer · **Effort:** medium

- [ ] The 100 Hz tick runs on **every** core. For a core on which exactly one compute-ready
      thread lies, every tick is pure loss — and in HPC the classic "OS noise" source
      that delays collective operations across thousands of ranks. Needed: arm the timer only
      if there is something to preempt (>1 runnable thread or an expiring budget), otherwise
      switch off. Together with [Z2](#z-target-architecture-as-of-2026-07-29--against-which-everything-else-is-to-be-measured)
      that yields: accounting by cycle stamp, preemption by need — the two are decoupled,
      and that is exactly the point.
- [ ] Core isolation as a policy: compute cores get no IPIs, no balancing
      (`balance_once` is off by default — here that happens to be right already), no
      interrupt delivery except their own.

### Z6. SMT — the gap that cache colouring does NOT close
**Class:** side channel · **Effort:** stages 0+1 done; stage 2/3 open

#### The road to SMT — five rungs, and which one blocks which

| # | What | State | What holds it |
|---|---|---|---|
| **0** | Read and report the topology | **done 2026-08-17** | `smt` line on both branches, 18 host tests |
| **1** | One physical core carries at most one logical CPU, fail-closed | **done 2026-08-17** | gates in `all_done()`; that it *bites* is measured by `tools/smt-messen.sh` |
| **1b** | The selftest image survives a machine with SMT | **open** | four conjuncts assume every configured core ticks |
| **2** | Name the trust domain | **open, blocked** | the decision in `docs/invariants.md` §12a |
| **3** | Gang-schedule siblings of one domain | **open, blocked** | needs rung 2 **and** a load model |

Rungs 0+1 give up **all** SMT throughput and buy the property outright. Rung 3 buys the throughput
back and **loses tenant-against-kernel** — that is the whole content of the §12a decision, and it is
why rung 2 cannot be written first. Rung 1b is independent of both and is pure test-image work.

**Priority against NUMA, decided 2026-08-17: rungs 2+3 come AFTER [Z8](#z8-numa).** Rung 1 already
bought the *security* property, so everything left of SMT is throughput work — and it then loses to
NUMA on three counts: SMT has a safe default and it is now installed, NUMA has none; SMT's cost is
bounded, known and chosen (×1.36…×1.89 forgone), NUMA's is unbounded and invisible; and rungs 2/3
are blocked on a decision and a load model while NUMA is blocked on nothing.

**Rung 1b is NOT deprioritised** — it is the one piece that gates everything else. Every server CPU
has SMT, so with stage 1 active the selftest cannot complete on real hardware at all. Cheap, and it
must come before any measurement on a real machine.

**Stages 0 and 1 are done (2026-08-17).** The topology is read (`CPUID.1Fh`/`0Bh` on x86,
`MPIDR_EL1.MT` on aarch64, decoded host-testably in `caprock_hal::smt`, 18 tests), and the
bring-up admits **at most one logical CPU per physical core**. Fail-closed: an unreadable
topology admits only the boot CPU, and `Unknown` is deliberately not `Single`. The `smt` line
gates in `all_done()` on both branches; the verdict is recomputed from the IDs that came online
rather than read back from the policy's own map. Normative text in `docs/invariants.md` §12a.

Measured, not assumed: `tools/smt-messen.sh` boots `-smp cores=2,threads=2` and gets
`topology=Multi{width:2,shift:1} logical=4 online=2 suppressed=2`, plus the counter-check that the
same kernel reads `Single`/`suppressed=0` under `threads=1`. In `tools/abnahme.sh` as `smt-politik`.

**The price, measured on 2026-08-17** (i7-13650HX, 3 runs each, throughput vs. one thread): a
sibling is worth **×1.36** on issue-bound work and **×1.89** on latency-bound work — the latter is
almost a second physical core, and it is where JIT'd/allocation-heavy target workloads sit. Stage 1
gives all of that up. That is the deliberate trade: *erst die Einschliessung, dann der Durchsatz.*

- [ ] **Stage 2 — name the trust domain.** Gang scheduling needs a unit, and the kernel knows PDs,
      not tenants (an app PD, its driver PD and the fs PD belong together). This must exist before
      the scheduler work, or the granularity is cast in concrete.
      **Blocked on the decision in `docs/invariants.md` §12a**: whether kernel entry joins the gang
      semantics (stun the sibling — expensive in an IPC-heavy microkernel) or the claim is narrowed
      to tenant-against-tenant in writing. That decision defines what a trust domain must mean.
- [ ] **Stage 3 — gang-schedule siblings of one domain.** As a **named reason** in `BlockReasons`
      (Z24), never a fourth bit and never a silent skip: a thread that is runnable and may not run
      is literally `sched_audit` code 7, the regression the D0 rebuild produced. The checker must
      count the **opportunity** (at every switch-in: does the sibling belong to the same domain or
      to nobody?), not the hit — at a rare event a hit-counting checker is silent in almost every
      run (the `pdbind` lesson).
      **Not before a load model exists.** The realisable share of the ×1.36…×1.89 depends entirely
      on the tenant mix: a domain with only one runnable thread forces its sibling idle and gets
      ×1.0 while still paying for the hardware. Building a mechanism whose value lies somewhere
      between 0 % and 90 % without knowing where is not engineering.
- [ ] **The selftest image assumes every configured core comes up.** With stage 1 active under
      `-smp cores=2,threads=2` the run ends in the watchdog with
      `offen waren: cores sweep ist verif` (`sched : core 1 ticks=0`). That is an assumption of the
      *test image*, not a kernel defect — but it means the suite cannot currently be run end-to-end
      on a machine with SMT. Either those four conjuncts derive their expectation from
      `caprock_sched::cores_online()` instead of the configured count, or the suite pins
      `threads=1` explicitly.
- [ ] **`iommu` and `iohealth` do not gate in `all_done()` on x86** (they do on aarch64; both are
      checked by name from `test-qemu-x86.sh`, so a red line does fail the run). Closing it needs a
      decision first: both lines may legitimately print `SKIP` on a platform without an IOMMU, and
      a conjunct that treats `SKIP` as `PASS` would be the absence-reads-as-success trap. Found
      2026-08-17 while wiring `smt`.

### Z6b. Debugger — capability-based, planned 2026-08-20
**Class:** tool/isolation · **Effort:** v1 small-medium (smaller kernel-side than Z6 or Z8),
v2 large

**v1 is built and measured (2026-08-20): `test-qemu-x86.sh` and `test-qemu.sh` both
`== ALL PASS ==`, seven `dbg` checks green.** Open is what stands below under "What is NOT built".
The six corrections that the measurement forced on the plan stand in
`docs/plan-debugger.md` under "Implementation status".

- [x] **v1: the mechanism.** `BlockReasons::DEBUG` (bit 6, type widened to `u16`),
      `debug_stop`/`debug_continue`/`is_debug_stopped`/`debug_release_pd`/`debug_stopped_count`;
      `ObjectKind::Debuggable` with **rights**-based derivation; third list in `Finalized` +
      `release_finalized_debug`; five syscalls (21..25), `ERR_DEBUG_BUSY`/`ERR_NOT_DEBUGGABLE`;
      `POLICY_DEBUGGABLE` in the signed manifest with **one** minting site; the third
      write stage (`writeback_erlaubt` + `frame_wort_setzen` on **both** architectures);
      `vspace_resolve` on both; `Freeze::Debugged`; migration lock; `dbg` line + 7 checks.

- [x] **The memory probe, arch-neutral** (`kernel/src/dbgmem.rs`, `dbgmem` line). It closes
      the one hole that the x86 green does NOT carry: `vspace_resolve` is the only place in
      v1 where the architectures really differ. **It immediately found a privilege
      escalation** -- without a `US`/`AP[1]` check a debugger with a bare `DebugRead` also resolved the
      shared KERNEL entries of the isolated VSpace and read kernel memory. From outside that looks
      like a working debugger.

- [x] **The counter-proofs** (`tools/dbg-negativ.sh`, seven mutations). Each checks three things: that
      the mutation TOOK HOLD, that the INTENDED conjunct fell, and with M3 in addition that the other
      stayed green -- the asymmetry is the statement. Two versions were wrong on the first attempt,
      and both errors stand in the plan: M1 missed the checked path (the target PD never goes through
      the loader), and M5 stayed green **because the ring word is gated twice** -- from that came
      M5a (prove depth) and M5b (gate at all).

- [ ] **What is NOT built:**
      * the **gdbserver PD** (RSP) -- and the order stands fixed: it relies more on
        `DEBUG_READ_MEM` than on anything else. Only when the path was unmeasured would every first
        error have been ambiguous (debugger or kernel path). Now it is measured.
      * v2: hardware breakpoints, single step, `hal::debug` -- **no debug surface in the HAL**,
        grepped on both architectures.
      * **The BAD case of the stop latency.** The good one is measured (128/128 valid probes,
        p50 ~6800, p99 ~13900 cycles, target on a foreign core) -- that is about 5 us, three
        orders of magnitude below the promise. But the bound `<= 10 ms` applies to a target core that has
        masked or stands in a long critical section, and **this series does not
        reach it**: a spinning thread accepts the IPI at any time. The bound remains DERIVED from the tick;
        the line says so itself.
- [x] **All three named gaps are closed** (2026-08-20). The user-window branch of
      `vspace_resolve` is measured along (2 MiB BLOCK: `PS` on x86, `BLOCK_DESC` on aarch64 -- the
      rights check sits there at a DIFFERENT place than at the leaf, and exactly in this half
      lay the found privilege escalation). The `dbg` probe lies arch-neutral in
      `kernel/src/dbgprobe.rs` and brings its own target -- thereby the progress is an
      **effect of the target** instead of a value that the measured path itself sets. And the
      worst case of the stop latency is **composed instead of measured a second time**:
      `longest IRQ-masked stretch (C9) + p99 <= one tick`, on x86 measured 71 per mille.

      **Three findings on the arch-neutral move, all triggered by the move itself:** the probe
      replicated a register layout (hard-coded word 18 -- on aarch64 a general-purpose register);
      it inherited on aarch64 an unchecked number (see below); and the `dbg` line was printed
      on aarch64 without anybody reading it -- literally the `pdbind` form. The guard
      `tools/berichtsgatter.sh` has since asked for arch-neutral modules **per architecture** and
      thereby immediately found a second instance (`dbgmem`, ungated on x86).

- [ ] **NEW and not from the debugger: `sperre`/C9 does NOT run on aarch64.**
      `hoechststand_bereinigt()` nevertheless delivers a number there (measured 5,446,459 cycles = 8720
      per mille of a tick), and the first version of the worst case took it -- red, and
      it looked like a serious debugger finding. **A bound from a summand that on
      this architecture nobody checks is no bound, whichever direction it comes out.**
      The line now reports SKIP with reason there. As long as C9 is missing on aarch64, the
      latency promise there is DERIVED and not measured.

- [ ] **The plan is `docs/plan-debugger.md`** (2026-08-20). Read that instead of this entry;
      what follows is the summary and the decisions that must not get lost.

      **The claim:** *debug authority is a capability over exactly one PD — delegable, revocable,
      auditable; a PD over which none was ever minted cannot be debugged, and that is checkable.*
      `ptrace` cannot express it.

      **v1 scope, kernel-side:** 3 new cap types (`Debuggable` / `DebugRead` / `DebugControl`) plus
      one that **already exists** (`ObjectKind::FaultHandler`, whose own doc names a debugger as a
      consumer) · 5 syscalls (21..25) · `ERR_DEBUG_BUSY = 19`, `ERR_NOT_DEBUGGABLE = 20` ·
      `BlockReasons::DEBUG` on bit 6 with the type widened to `u16` **in the same change** · an
      extended write mask · a third list in `Finalized` · **6 judging places** · one object
      generation · one `dbg` line. ~750 lines kernel-side, ~1500 in an unprivileged PD speaking
      GDB's RSP. Estimated, and marked as such in the plan.

      **The three things that bite:**

      1. **The mint policy IS the claim.** Minting `Debuggable` by default makes the claim vacuous
         — no such PD would exist. So *not* minting is the default, and the place for the decision
         is `POLICY_DEBUGGABLE = 1 << 4` in the **Ed25519-signed, kernel-hash-bound** manifest,
         enforced in `loader.rs:1196 policy_gate`. That makes "who may be debugged" attested and
         anti-downgrade-protected, not a runtime switch. **Custody is the other half:** the window
         ends by revoking `Debuggable` itself — time-limiting a derived cap does not close it.
      2. **`revoke` must not brick the target.** A DEBUG-stopped thread whose debugger dies keeps a
         reason bit nobody may clear. **The mechanism already exists and is not the one the first
         draft invented:** `cap_delete` (`system.rs:2519`) and `cap_revoke` (`:2536`) are
         structurally identical and both call `abort_finalized_replies` (`:2554`) with **no lock
         held** — which is this exact problem, already solved, for `CALL`-blocked callers whose
         Reply cap was finalised. The release is a **third list in `Finalized`**, drained beside it.
         One copy, both paths, existing lock order, and `note_finalize_overflow` already shouts when
         the capacity is exceeded.
      3. **The debugger needs a *third* write authority** — GPR **+ PC + SP + masked flags**. And
         measured 2026-08-20: the limit must come from the **capability**, never from `KOPF_NGPR` in
         the handler-writable sidecar page; `kopf_pruefen` compares that word against the reader's
         own number and rejects on mismatch, so a debugger's wider mask cannot ride on it.

      **Two things the plan found already built**, which is why v1 shrank: `KOPF_GEN` (the stop
      generation asked for) and `FaultHandler` (crash catching). **One thing that does not exist at
      all:** any debug surface in the HAL — grepped 2026-08-20 for `MDSCR`, `DR7`, `debugctl`,
      `PSTATE.SS`, zero hits on either architecture. That is v2's largest item.

      **Prerequisite that lands alone, before v1:** `detach_for_migration` must refuse a thread not
      in a ready queue. A DEBUG-stopped thread cannot migrate today, but only **incidentally** —
      `migrate_to(tid, dst)` checks no state, and the property rests on caller discipline.

      **Deferred, and coupled to a product decision:** software breakpoints patch program text, and
      W^X here is not merely a report line but a **Verus proof** (`verus/wx_invariant.rs`). Route 1
      (hardware breakpoints only) carries v1 and v2; the customer-debugging path (Velve ch. 16,
      ~50 breakpoints per session) needs route 2 — a distinct text-write capability with open
      windows counted in the `wx` line — and route 2 now carries a **proof** obligation, not just a
      measurement one.

      **Corrections recorded in the plan's appendix A** rather than dropped, including: „zwei
      `SCHEDS` werden nie gleichzeitig gehalten" was **wrong** (`migrate_to` holds two, already in
      ascending-index order; the two comments are scoped to `KernelSched` `:924` and
      `least_loaded_core` `:2720`), and the first draft's "kick the target core" release is
      superseded by the `Finalized` path above.

      The ordering rule behind all of this is `docs/invariants.md` **§1b** — three instances
      (D0, C8, debugger) make it a rule, with a named requirement for Gabbro: a pairing that makes
      the wrong order **unspellable**, not a lint that finds it.

### Z7. Attestation and measured boot
**Class:** trust · **Effort:** medium-large

- [ ] The certificate gate (ADR 0014) secures which **binaries** may start. A tenant
      however wants to be able to prove remotely **what runs on the machine** before they give
      their state there — measured boot, measurement chain into the kernel, signature over the
      measurement. Precondition of [Z4e](#z4-checkpointrestore-of-a-thread); without it
      thread migration is a leap of faith to an unknown machine.

### Z8. NUMA
**Class:** memory/scaling · **Effort:** medium, reaches into the allocator

**Measured state, 2026-08-17 (grepped, not remembered).** One thing exists, everything else does
not:

| Piece | State |
|---|---|
| ACPI **SRAT / SLIT / HMAT** | **read nowhere.** The tables actually parsed are `APIC`, `DMAR`, `MCFG` — that is the complete list in `x86_64/acpi.rs` |
| aarch64 device tree `numa-node-id` | not read either |
| `PhysAllocator` free list | flat, **no node concept** |
| Scheduler | no notion of which node a core belongs to |
| Manifest field `numa_node:u32` (offset 68) | **exists, and is fail-closed** |

So the honest summary is: **the format carries the field, the kernel refuses it, and nothing else
is built.** `loader.rs` rejects any entry with `numa_node != 0` and prints why, rather than
silently placing the program anywhere — checked as `ladepol`. That refusal is the *only* NUMA work
that is done, and it is the right one to have done first: a field that is never honoured collects
unchecked values, and the day it is honoured is the day they are all wrong (measured at A1, where
priorities had sat unfulfilled in the test manifest for a year).

**Two things that are already in the right shape for it**, so this is less green-field than it
looks: `alloc_colored_below` decides colour **and** zone in *one* call and its doc comment already
names this entry as the reason; and `enum Zone` (`kernel/src/system.rs`) is the single place where
placement classification lives. A node is a third axis on an existing decision, not a new layer.

**And unlike CAT/MPAM, it is testable.** The development machine has exactly one node, so the case
cannot arise there — but QEMU presents a full topology (`-numa node,nodeid=…,cpus=…,memdev=…` plus
`-numa dist,src=,dst=,val=`, verified 2026-08-17). The CAT entry was deferred because it was
*buildable but not verifiable*; that argument does not apply here.

#### The road to NUMA — five rungs

Same shape as the SMT ladder above, and deliberately so: what made that one work was that each rung
carried its own falsifiable line rather than a promise about the next one.

| # | What | State | Blocks |
|---|---|---|---|
| **N0** | Read the topology (SRAT/SLIT, DT `numa-node-id`) | **done 2026-08-17** (x86 measured; aarch64 built but unexercised — see below) | N1 |
| **N1** | Classify every RAM range to a node, fail-closed; report line `numa` | **done 2026-08-17**, gates on both branches | N2 |
| **N2** | Node as a constraint on the **one** placement decision | **partly done** — node+zone yes, colour axis open | N3, N4 |
| **N3** | Scheduler: a thread runs on a core of the node that holds its memory | **open** | N4 |
| **N4** | Honour `numa_node` from the manifest instead of refusing it | **open** | — |

##### What landed on 2026-08-17, and what it measured

`caprock_hal::numa` decodes SRAT (all four affinity subtable types, including the **split**
proximity domain of the legacy APIC entry) and SLIT, plus a `numa-node-id`/`reg` walker in
`caprock-dtb` that reports through callbacks so both architectures share one set of truncation
rules. **15 host tests.** `kernel/src/numa.rs` holds the one topology, prints the `numa` line and
answers "which node"; the line gates in `all_done()` on both branches and both suites check it.

Measured by `tools/numa-messen.sh` (in `tools/abnahme.sh` as `numa-topologie`):

```
-numa 2 Knoten:  nodes=2 ranges=3 cpus=4 distances=true trustworthy=true covered=2047 MiB
                 distance(0,0)=Some(10) distance(0,1)=Some(21)   <- der Wert aus -numa dist,val=21
                 placed exact=3 off_node=0 refused=0 speaking=true
ohne -numa:      readable=false nodes=0  -> und trotzdem ALL PASS
```

The counter-check is the second line: a decoder that always claimed two nodes would pass the first
run and fail this one.

**N2 has a real caller, on purpose.** The AP stacks are allocated through the ladder
(`numa::alloc_on_node`), which is why `speaking=true` above is not a promise. Arithmetic with no
caller is the trap the Z26/A3 sidecar work already paid for: two mutations proved the *functions*
were right while nothing called them.

##### Three limitations of what landed — named, not glossed

- [ ] **The colour axis: the ladder STANDS, the real caller is missing** (state 2026-08-20).
      Built are `numa::alloc_on_node_colored` (three rungs `Exact -> OffNode ->
      OffNodeUncolored`, each counted by EFFECT), `system::alloc_colored_in_window` and
      `mem_alloc_masked_auf` as the place where mask and node come together.
      `off_node_uncolored` is thereby no longer structurally `0`.

      **What is missing is exactly one piece of wiring, and it is named:** `LadePolitik` carries
      `farbig` and `core`, but **no node** -- `ladepolitik_auf` resolves `numa_node` at once into
      a core. As long as that is so, nobody calls `mem_alloc_masked_auf` with a real node,
      and the ladder is arithmetic without a caller -- **the same trap that Z26/A3 has
      paid for** (two mutations proved that the functions are right while nobody called them).
      That is why an open hook still stands here and no success report.

      The step: `LadePolitik { node: Node }`, set from the manifest, passed through to the
      coloured allocations of the load path. After that "colour yields last" is a measurement.
- [ ] **On aarch64 the reader cannot see a runtime topology at all.** `kernel_main(dtb_addr)`
      **prints** the pointer QEMU passes and parses the **build-time embedded** `virt.dtb`
      (`include_bytes!`). That is pre-existing and affects `cpu_count`/`memory` too — but it means
      the DT NUMA walker is *built and unexercised*, and no `-numa` run could exercise it. Either
      the kernel starts parsing the DTB it is handed, or the aarch64 half of N0 is honestly marked
      untested. **Do not read the green aarch64 `numa` line as evidence for the walker**; it only
      shows the reading happened and correctly reported "no topology".
- [x] **N4's positive path is now driven by the load suite** (2026-08-20) -- and on the first run it
      delivered a kernel finding that the negative case structurally could not show:
      **`numa::init()` ran AFTER the loader.** In the same output stood
      `numa : readable=true nodes=2` and `loader : ... readable=false`; the loader line was not
      wrong, it was asked too early. Fixed by the order **and** a gate against
      somebody turning it back: the loader now distinguishes THREE situations -- "not yet
      read" (boot-up error, loud), "no topology" (the machine), "node missing" (the
      document). Until then the first two coincided.

      In addition two set-up fixes: `boot()` takes `-m`/`-smp` from `MEM_ARGS`/`SMP_ARGS` instead of
      wiring them in, and **QEMU's messages no longer go to `/dev/null`**. The value of that
      was immediately measurable: my first `-numa` call was wrong (`node,mem=` instead of `node,memdev=`),
      and without the stderr lines it would have become "the kernel does not read the SRAT" -- i.e.
      exactly the mix-up because of which the case stood open for a year. The call form is now
      COPIED from `tools/numa-messen.sh`, not invented anew: two call forms for one
      property drift.

- [ ] ~~N4's positive path is not driven by the load suite.~~ `boot()` in
      `test-qemu-x86-load.sh` hardwires `-m "$RAM" -smp 4` *before* any extra arguments, and a
      `-numa` topology needs `-smp sockets=…` plus matching `memory-backend` sizes at exactly those
      places; a second `-m`/`-smp` afterwards collides. Measured 2026-08-17: QEMU starts, the guest
      gets **no SRAT** (`numa : readable=false`), and the resulting line reads like a kernel finding.
      `boot()` sends every QEMU message to `/dev/null`, which is what makes the confusion possible —
      the same shape as the `iommu_platform=on` split. What *is* driven: the negative case (a node
      wish on a machine without a topology gets its own reason), and the whole positive read/place
      path via `tools/numa-messen.sh`.
- [ ] **`Topology` is copied out under a lock on every query** (`node_of_pa`, `node_of_core`). Fine
      at bring-up rates, wrong for a hot path — N3 puts it on the placement path, and then it needs
      a lock-free snapshot or the C9 lock-hold mark will find it.

**Testability is settled, and it was measured before planning** (2026-08-17): `qemu-system-x86_64
-machine q35 -numa node,… -numa dist,…` emits **SRAT and SLIT** into guest memory (dumped and
grepped, 1 each, alongside the `APIC`/`MCFG` this kernel already parses), and `qemu-system-aarch64
-machine virt` emits `numa-node-id`, `distance-map`, `distance-matrix` into the device tree
(`dumpdtb`). The development machine has exactly **one** node, so the case cannot arise there — the
emulation is the only way to see it at all, which is precisely why the decoders must be pure
functions over **injected** bytes (the DMAR rule: a test against the real table is an oracle that
holds because its antecedent is false).

**What the suites can NEVER show: the benefit.** QEMU hands out topology, not latency — a guest's
"remote" node is ordinary host memory at ordinary host speed. Verifiable is **where a page came
from and where a thread runs**; the speedup is not, and no green line here may be read as one.
Exactly the SMMU-under-QEMU distinction (§6) and the SMT one (§12a). Measuring the benefit needs a
real multi-socket box — a rented dual-socket EPYC for one afternoon would settle it, and until then
the honest claim is *placement is correct*, never *it is faster*.

##### N1: what "fail-closed" has to mean here

SRAT need not cover all of RAM (firmware routinely leaves ranges out). The bequeme Fassung assigns
those to node 0 — and that is `Unknown`-as-`Single` all over again: a range nobody classified would
be indistinguishable from a range classified as local. So unaffiliated memory gets its **own**
value (`NodeId::Unaffiliated`), the report line prints its size in bytes, and it is allocated from
**last**. A machine where that number is large is a machine whose topology we do not actually know,
and the line must say so rather than average it away.

##### N2 is the hard rung, and the risk is a THIRD axis on one address

Colour (cache sets), zone (below/above 4 GiB) and node all constrain the **same** physical address,
so they must be decided in **one** call — `alloc_colored_below` already fuses the first two and its
doc comment already names this entry as the reason. Adding a node makes the constraint set small
enough that it will regularly be unsatisfiable, and then **the yield order decides everything**. It
must be named, not emergent:

1. **Zone never yields** — it is correctness, not preference (`vspace_map_page_at` refuses
   `va >= GIB1_END`).
2. **Node yields first** — it is performance.
3. **Colour yields last** — it is the A1 isolation claim. A silent colour yield would be the
   `MASK_BITS` failure again: green because nothing was separated.

And every yield is counted **by effect, not by attempt**. The E-Rest 3b fallback counter reported
`1x` on a 512 MiB machine that had no high memory at all — it had counted an oversized request that
fitted nowhere. "There was no room on the node" and "it was taken from another node" are two
statements.

*Warning from the same family:* "unten zuerst" was for years an accident of the size relation
rather than a design. A three-axis best-fit will produce exactly that kind of property — one that
holds until the fragment sizes change. Whatever N2 guarantees must follow from the structure, and
the report line must carry the number that would show it slipping (pages allocated off-node).

##### N4 is the day the field's values are all wrong

`numa_node` has sat in the manifest format collecting unchecked values since it was defined, and is
today **refused** whenever it is non-zero (`loader.rs`, checked as `ladepol`). That refusal is why
this is currently safe. When N4 honours it, every existing entry must be re-examined, and the test
manifest must carry a non-zero value that is actually **satisfied** — not merely accepted. This is
the A1 finding verbatim: the priorities sat unfulfilled in the test manifest for a year, and the day
they were honoured the same allocation tore the load suite apart.

##### Where it collides with work that already exists

* `least_loaded_core()` minimises TCB slots and now also skips offline cores (Z6 stage 1). N3 adds a
  third criterion, and the same care applies: `core_node(c)` must be its own named quantity, not a
  meaning smuggled into the load counter — *ein Parameter, der zwei Bedeutungen traegt* (D9,
  `spawn_user`).
* `balance_once()` migrates on a difference of `IMBALANCE_THRESHOLD = 2`. A **cross-node** migration
  costs more than that imbalance saves; it needs its own, higher threshold, or it must not happen at
  all.
* `enum Zone` (`kernel/src/system.rs`) is the single place where placement classification lives. A
  node is a third axis there, not a new layer.

**Priority: ahead of [Z6](#z6-smt--the-gap-that-cache-colouring-does-not-close) rungs 2+3**
(decided 2026-08-17) — see the reasoning recorded there. In one line: SMT already has its safe
default installed, NUMA has no safe default at all.

- [ ] The `PhysAllocator` has **one flat free list without a node concept**. With the target picture
      dual EPYC that is no detail: memory at the wrong node costs roughly a factor of two in
      latency, and with CXL it gets worse. Needed: read nodes from ACPI SRAT/SLIT, free lists per
      node, PD allocation node-local, and the scheduler must not put a thread on a core
      whose node does not hold its memory.
- [ ] **Interlocked with A1**: colour *and* node must be allocated **together**. Two
      independently developed policies otherwise fight each other — the colour stripe enforces
      one physical address, the node another, and whoever allocates first wins. That belongs in
      **one** decision, not in two layers.

### Z9. Fault domain
**Class:** operation · **Effort:** design decision

- [~] **The stipulation stands (B-6.2, 2026-08-02) — and the premise of this entry was false.**
      "A kernel panic takes the node down" is **not** true, and that is the worse news:
      the panic path holds the core **without IRQ masking**, the next timer tick
      brings it back into the scheduler. Measured, the node ran **61 s on** and reported
      `ipc`, `ring3`, `iommu`, `dmatok` as ALL PASS — with a demonstrably violated
      invariant. Four different outcomes depending on **where** the panic occurs, among them a
      silent total failure (under the MEM lock) and a state that cannot be told from a
      deadlock from outside.

      Stipulation in `docs/fehlerdomaene.md` (operator-side, with measurement table and VM comparison),
      normative as `docs/invariants.md` §14.

      **What is open is the decision, not the description:** the stipulation is today
      *claimed*, not *produced*. Three measures of about half a day together make
      it true — mask IRQs in the panic path, a recursion guard in the panic handler,
      `panic → system_off` (both shutdown ways exist and are **never** called from `panic.rs`). That is a weighing between availability and honesty and belongs to Simon.

### Z10. I/O at all
**Class:** foundation · **Effort:** large

- [ ] No network stack, no file system. A cloud without network and storage is none.
      *(Two steps done, 2026-08-01: **A-5.2** brought driver logic for block device and
      network card, core-free in `crates/caprock-virtio`; **A-5.1** brought the first driver PD —
      `programs/hardware/virtio-blk` reads a sector as a loaded userland program, with
      its own device allocation from the manifest.)*
      Since 2026-08-02 the driver is a **service**: it serves requests via its channel and
      is exchangeable in operation (A-5.1 completed).
      Since 2026-08-02 above it stands a **storage stack, entirely outside the kernel**:
      block service (A-6.1), GPT (A-6.2) and a reading **FAT16 file system as a PD of its own**
      (A-6.3). A file is read via GPT → FAT16 → block service → driver.
      Since A-6.4 it also **writes** (second cluster, both FAT copies, flush, every byte
      read back; independently re-checked at the image).
      **What continues to be missing:** create/delete files, subdirectories, long names — and a
      **network stack**. Every driver **polls**: an IRQ cap cannot be granted as long as there is no
      IRTE allocation (B-3).
- [ ] For passed-through devices **interrupt remapping** ([E](#e-dma-hardening--remainder), step 4)
      is no freestyle: without IR a device can generate arbitrary interrupt messages. That is the
      standard breakout from a device assignment and must stand before the first tenant device.

---

## A. Open from the security review (ext-29)

### A1. Cache/timing side channels between PDs
**Class:** side channel · **Effort:** architecture change, no patch

`[~]` **Stage 1 stands and is measured on BOTH architectures** (see done.md):
cache geometry is read from the HW, the allocator allocates colour-pure, and two PDs created that way
demonstrably share no cache colour — region, kernel stack and page tables. x86 since
2026-08-01 (since 2026-08-02 with the REAL geometry of the machine, 512 colours — before that
QEMU reported 256 invented ones), aarch64 since 2026-08-02 (16 colours — the split on which the
`MASK_BITS` mix-up hung). What follows remains open.

- [ ] **Way partitioning (Intel CAT / AMD L3 QoS / ARM MPAM) — assessed 2026-08-07, not
      built, and the reason is a measurement.** On the development machine it does not exist:
      13th-gen Core i7, no `cat_l3`/`rdt_a` flag in `/proc/cpuinfo`, no `resctrl`. It could
      thus be built here, but **not checked** — and an assurance without measurement is in this
      project no progress, but a line in `docs/invariants.md` that nobody can
      redeem. (On the product target dual EPYC there is L3 CAT; there it would be measurable.)

      **The design point that applies independently of that:** colouring and way partitioning solve
      the same problem at different levels, and **both at the same time without a common policy
      is worse than one**. Colour restricts which *sets* a PD can occupy; CAT
      restricts which *ways* it may occupy. Two independently developed allocators fight
      against each other — the same trap as colour against NUMA ([Z8](#z8-numa)), where it is expressly
      named. Whoever introduces CAT must first decide whether it **replaces** the colouring
      (then `region_bytes()` falls away and PDs may take large blocks again) or **complements** it.

      The advantage of CAT would be exactly what the colouring lacks: no binding to physical addresses,
      thus **effective also as a guest** — see §12, where it is measured that colouring does not carry at all under KVM
      (`disjunkt=234` against `gleichfarbig=210`).

- [ ] **Way partitioning (Intel CAT / ARM MPAM) not considered.** It would hit the same
      property via the hardware instead of via the allocator, would get by without smaller regions and without
      page-wise mapping — and could be combined with the colouring. QEMU does not emulate CAT,
      so it would be **not checkable** here; that is a reason to defer it, no
      reason to forget it.
- [ ] **A1 separates the LLC and nothing else.** Core-local structures (L1, L2, TLB, store buffer,
      branch predictor) remain shared as soon as two tenants lie on sibling hyperthreads —
      see [Z6](#z6-smt--the-gap-that-cache-colouring-does-not-close). And the colour allocation must be decided
      **together** with the NUMA allocation, not one after the other, see
      [Z8](#z8-numa).

- [~] **The proof of effect is built, but not delivered** (B-4.5, see
      [todo-B-verlaesslichkeit.md](todo-B-verlaesslichkeit.md)). The Prime+Probe runs
      arch-neutrally at **every** start and is read by both suites; set-up,
      positive control, colour choice and balance are checked also where no verdict is possible —
      a check path that runs for the first time on the target day is broken on the target day.

      **The old assumption "only on bare metal OR under KVM" was wrong:** under a hypervisor
      the question is not hard to measure, but **in principle undecidable** — a guest colours
      guest-physical addresses, and the second translation stage rewrites the colour bits. Measured
      (guest): `ungestoert=41 disjunkt=234 gleichfarbig=210` — the positive control carries, the
      disjoint colour set does **not** protect.

      **Open:** a run on real hardware — and the machine must fulfil a condition:
      `size of the non-partitioned cache level < LLC / PARTITIONS`. Otherwise there is **no
      valid victim size**, and that is a property of the machine, not of the test.

### A2. Spectre v2 on HW without FEAT_CSV2
**Class:** side channel · **Effort:** small, but HW-dependent

`array_index_nospec` (v1) and the barrier at the VSpace switch are there. For v2 on HW that does **not**
report `CSV2 >= 1`, branch predictor invalidation at the address-space switch would be needed —
`SMCCC_ARCH_WORKAROUND_1` via SMC. QEMU `virt` does not offer the firmware call → only
implementable/testable on real HW (STM32MP257F-DK). The boot report `spec :` already shows what the HW guarantees.

### A3. The global cap table remains a shared resource of fixed size
**Class:** fairness/scaling · **Effort:** medium-large

`CAP_BUDGET_PER_PD` now bounds the **damage**, but does not replace the seL4 model (every PD
gets its CNode from its **own** untyped budget). As long as the table is shared, capacity
remains a global quantity.

**Partly done on 2026-08-26: the constant has become an ACCOUNT.** Every PD carries its
own number (`Pd::cap_budget`), which is booked at creation against a **stock**
(`PdTable::budget_vorrat`, initially `CAP_SLOTS_FOR_ALL_PDS`) and given back at teardown;
`SYS_LOAD` carries the request in `MSG3` (`0` = default). Measured as
`budget` in every x86 run: `Vorrat 80000 → 79992 → 79980 → 80000`. The load-bearing line is the
**return** — a stock that only shrinks looks like one under load.

The reason for the movement is a number: raising a constant costs the need of **one** PD
times `NPDS`, i.e. about 7.7 MB per eight slots, for a need that a single PD has. The same
movement as with `MELDESTELLEN` (kept by hand → derived) and `IDENTITY_DEBTS` (number → set).

**Two things remain open, and the first is bigger than the entry:**

- [ ] **`NCAPS = 16` is the real cap per PD, not the budget.** The local cspace is an
      array of fixed length; `install_cap` rejects every slot `>= NCAPS`, **without asking
      the budget**. `CAP_BUDGET_MAX` stood at 64 and was thereby an unfulfillable promise — the
      self-test refuted it on the day of introduction (it filled up to 16 and failed at the
      slot bound, not at the budget). Now `CAP_BUDGET_MAX == NCAPS` with `const assert`.
      For a driver environment with thirty caps a **variable** cspace per PD is needed —
      see TODO.md Part II K1c.
- [ ] **The rejection at an EXHAUSTED stock is unmeasured.** To trigger it would mean allocating about 80,000
      slots, i.e. thousands of PDs — a measurement that would shift the baseline of every other test
      of the run. Measured instead is that the **two reasons are counted distinguishably**
      (`budget_abgewiesen` demonstrably does not rise at "above `CAP_BUDGET_MAX`"). The path
      itself belongs in `tools/kapazitaet-messen.sh`, where thousands of PDs arise anyway.

**Measured in addition (2026-08-10, see [C7](#c7-the-capacity-curve--where-it-really-breaks-measured-2026-08-10)):
the shared cap table is today NOT the bottleneck — at 9984 simultaneous processes 11
of 80,256 slots stand.** That weakens the urgency, not the argument: the peak hangs
on how many caps a PD **holds**, and the curve PDs hold none. A number that comes from
a load case without caps says nothing about a load case with caps — the same distinction as
`rx_used` against "data have arrived".
**Side condition when enlarging: done** (A-3.3). `ReplyFinal`/`Finalized` held its
`[(u32,u64); NOBJECTS]` array **on the kernel stack**, and not as the only one: `dma_finalize` held
a copy of the same regions, every `finalize` implementation of the enforcers once more a
`[usize; NOBJECTS]`. Measured at the x86 release the frame of `cap_delete`/`cap_revoke` was therefore
**6168 bytes** — large enough that the compiler built in a stack probe. Now the structure borrows
its memory from the caller (a static buffer in the kernel), frame 72 resp. 88 bytes. Raising `NOBJECTS`
is thereby no longer coupled to the stack size. See also
[C3](#c3-cap--pd--ipc-tabellen-dynamisch).

*(The earlier statement "currently 2 KiB" was outdated: kernel threads have 64 KiB, EL0 threads a
16 KiB EL1 stack. The point stood nevertheless — 6.2 KiB were 38 % of the smaller of the two.)*

### A3c. The description texts of the suites are EXECUTED
**Class:** tool · **Effort:** small, but 13 places

`check "$muster" "$beschreibung"` is called with a **double-quoted** string, and
backticks in it are executed by the shell. Measured 2026-08-26: **13** messages
`./test-qemu.sh: Zeile NNN: dbg: Kommando nicht gefunden` per aarch64 run — the descriptions
quote check lines like `` `dbg : FAILURES` `` and `` `== ALL PASS ==` ``, and the shell tries
to start them.

Consequence-free today (the substitution yields empty text, the error goes to stderr). The condition
behind it is not: **a description is text, no program.** Whoever in future writes
`` `git rev-parse HEAD` `` into a description gets different behaviour without comment —
and whoever accidentally quotes something destructive gets it executed. That these lines
produce noise has in addition scattered 13 lines of error message into every log in which one
searches for real errors.

Fix: descriptions in **single** quotation marks, or replace the backticks by typographic
characters. Do not change during a running suite (bash reads scripts incrementally).

### A3d. `dma_audit` reports **2** after a hot reload — and the cause is unexplained
**Class:** DMA/teardown · **Effort:** small to measure, unknown to fix

Measured 2026-08-26 in the load suite, at the end of the run: `dma_audit() == 2`, i.e.
"two DMA regions overlap physically" (`dma_bounds_audit`, pairwise disjointness).

The cause is the A-5.1 exchange: `reassign_driver_device` mints the new driver version a
cap over the **same** region — that is intended, it is supposed to inherit it. As long as the old cap still
exists, the checker sees two objects on one range. Its rule ("different objects
must never overlap") does not know the **hand-over**.

**The obvious reading "blind spot of the guard" is REFUTED** (2026-08-26, from the
source text, not conjectured):

* `for_each_dma` runs **object-** and not cap-granular, and the comment there says why:
  "so that cap copies do not count the same region several times (otherwise false-positive
  self-overlap)". The case "one region, deliberately held twice" is thus **already
  exempted** — for copies.
* `reassign_driver_device` however makes no copy: `install_dma_cap_ex` → `install` →
  `alloc_object` creates a **second object**. `2` thereby means exactly what the rule says.

**Thereby the core is not the cap, but the IOMMU registration.** Even with a correct refcount
over the region the following holds: as long as the IOVA→PA mapping stands, the device writes into the pages —
regardless of who owns them in the meantime. The order that must hold is
**stop device → IOMMU unmap → memory release**; the `DmaRegion` teardown token (`dmatok`)
is built for exactly that. `reassign_driver_device` calls **no `dma_attach`** — the IOMMU registration
continues to belong to the *first* object, and on its finalisation (`refcount == 0`) `space.rs`
pushes the region via `push_dma(phys, len)` into the release path, while the second object and
the translation still stand.

**Two fixes, and the first is presumably the right one:**

1. `reassign_driver_device` **copies** the existing cap (`cap_copy`), instead of minting a second object.
   Then the refcount goes 1→2, the overlap disappears, and the finalisation
   runs exactly once — at the last `delete`. `dma_audit` would thereby go to 0.
2. If reason 1 does not carry (e.g. because the new version needs different rights/direction): the
   teardown token must go through the exchange, not bypass it.

**The size relation can be ENFORCED, instead of waiting for it** — the same method as with the
`admit` window: after the reload deliberately make a request that falls into the released range,
write a magic pattern into it, let the device run DMA, check the pattern. A test that
**produces** the condition, instead of hoping that the allocator hits it some day.

**As long as it is open, `dmapool` carries the number WITH AN EXPECTATION** (`dma_audit == DMA_AUDIT_SCHULD`,
today 2) — not as `== 0` (red for a foreign reason) and not as a mere number in the text (a number
whose expected value is 2 is not checked: if it became 3 for a real reason, nothing
would fire). The constant is a **ratchet downward**: it goes to 0 as soon as the fix stands,
and whoever raises it covers a new overlap.

### A3e. The three remainders from K1a/K1b are DEBTS, no measurement gaps
**Class:** naming · **Effort:** the first two small, the third its own

After the completion of K1a/K1b three items remained standing. They were noted as "not measured",
and that is the wrong category for two of them — **a measurement gap is closed when
somebody measures; a debt applies until somebody builds.**

1. **The `admit`/`STACK_CAP_OF` window belongs to M1, not to "later".** Two threads of the same
   PD on different cores can both check before the respective other entry stands — the
   overlap check that M1 of the counter-proofs switches off thus has **itself** a hole, and exactly
   in its subject. `ueberlappung-abgewiesen=true` is thereby a statement about the sequential case.
   That belongs in the line and in `spawnarena-negativ.sh`, not in a future entry. To close it would mean: reserve the sub-region **before** the spawn and release it on
   every failure — own bookkeeping with its own abort paths.
2. **Guard pages between the arena windows** — the half of the price that is not paid: the
   allocation rejects overlap, an **overflowing** stack continues to wander silently into the
   neighbour. A measurement of its own, no side effect: on aarch64 the guard does not exist in hardware
   (`guard_unterstuetzt() == false`), and a page that does nothing there has once already
   made a colour promise structurally unfulfillable (C9e).
3. **The exhaustion path of the slot stock** is really only a measurement gap — it needs thousands of
   PDs and belongs in `tools/kapazitaet-messen.sh`, where they arise anyway.

### A3b. `tools/kernel-grenze.sh` is RED (state 2026-08-26)
**Class:** kernel boundary · **Effort:** small per module, but it is five decisions

Five modules lie in the HAL and stand neither on the allow list nor as a named exception:
`fbtext`, `iommu_health`, `numa`, `bootparams`, `smt`. **Pre-existing** — neither the guard nor the
HAL has been changed since the last green state; the modules arrived individually, and
nobody ran the guard in the process.

> **A gate that exists and is not run is none** — the same line as with
> `hal::mmu::guard_*` on 2026-08-12. The acceptance (`tools/abnahme.sh`) runs it; a branch that is
> merged without it gets past it.

Each of the five needs a **separate** decision — userland driver PD (A-5.1) or a named
exception **with justification why the kernel itself needs it**. A collective admission is exactly the
way on which a microkernel becomes a monolith; the guard says that in its own
error message.

First suspicion per module, not checked: `bootparams`/`smt`/`numa` are boot-up information (could
carry), `iommu_health` belongs to the IOMMU facade (likewise), `fbtext` is an **output driver**
and thereby the candidate that really belongs outside.

### A4. No syscall to delete own caps
**Class:** ABI gap · **Effort:** small-medium

The ABI knows no `CDELETE`/`CMOVE`/`CCOPY` — a PD's cap slots are cleared only by the kernel (teardown,
PDCTL). Consequence: a long-lived service that receives caps dynamically (IPC grant) runs against
`CAP_BUDGET_PER_PD` and **cannot free itself**. Needed: at least `SYS_CDELETE` (own
slot, cap-gated onto the own cspace), sensibly also a selectable receive slot for
grants instead of the fixed `GRANT_RECV_SLOT`.

---

## B. Thread migration — remainder

**Goal:** threads are no longer bound to the core on which they were created.
**Blocker was:** `ThreadId.slot` encoded the core (`slot / PER_CORE`) — the identity of a thread
hung on its core affinity.

- [~] **B4** Policy present (`balance_once`, in the tick path behind an interval), but by
      default **OFF** (`system::set_balancing`). **Open:** switch on by default. Blockers are
      not the mechanism, but the demo/test threads of this image that presuppose fixed affinity
      (cross-core IPC test, budget donation, placement telemetry) — they would first have to be formulated
      independent of affinity.

**Likewise open:** migration of a thread with active **budget donation** (the donation links
are local slots — today the migration is in that case simply refused until the server
has replied). For a cross-core donation the links would have to become global `gid`s.

**Deliberately NOT migrated:** the running thread of a core (only ready/blocked) — otherwise
the trap frame would have to be taken over across cores.

---

## C. Flexible capacities (target picture: 256 cores, many thousand processes)

**Target picture:** dual-EPYC class — **256 cores**, **many thousand** processes, efficient.
The thread/scheduler side has been converted (capacity comes at boot from the RAM, hot paths
O(1); proved by the `scale` test with 1024 simultaneous threads). Open remain the cap/IPC
tables, the GIC scaling and the x86 port.

- [ ] **C3** <a id="c3-cap--pd--ipc-tabellen-dynamisch"></a>Cap/PD/endpoint/notification tables
      dynamic (hung on [A3](#a3-the-global-cap-table-remains-a-shared-resource-of-fixed-size):
      detach `ReplyFinal` from the stack first — **that has been done since A-3.3**, the way is free).

### C4. Efficiency with thousands of threads (eliminate linear scans)

Capacity alone does not suffice — several paths are **O(n)** in the table size and become a
bottleneck with thousands of threads:

**Since 2026-08-10 there is a measured baseline for it instead of a list** — the line
`vorrat` prints fill levels **and** iteration counts in every run (`tools/boot-x86-log.sh`).
Counted, not timed: an iteration count is a property of the program, a
time measurement is not (D10).

- [ ] `CapSpace::{free_slot_index, alloc_object}` — linear scans → free lists.
      **Baseline:** in the capacity curve never the bottleneck — at 9984 processes the
      cap slots stand at **11/80256**. The table is overdimensioned for this load case by three orders of magnitude;
      the scan is nevertheless linear and will become so under cap-hungry PDs.

- [ ] `PdTable::create` — linear scan → free list. **Baseline: 16 iterations** in the normal
      suite run; in the curve it grows quadratically with the PD count (every creation scans from the
      front to the first free slot).

- [ ] `purge_ipc_queues` — iterates **all** endpoints + notifications per thread death.
      **Baseline measured: 6 calls → 120,768 traversed IPC objects**, i.e. 20,128 per
      thread death (`NEPS + NNTFNS`), each individually locked. With 10,000 dying threads that is
      **201 million** lock operations. That is the largest remaining number of the list.

      **It is the same structure as `pd_of` before its fix: the kernel SEARCHES who points at
      the dead one, instead of KNOWING it.** And the structural version is the same — an
      **intrusive wait list per thread**: whoever blocks enters itself at the object *and* at itself;
      the death walks the **own** list. Thereby the path becomes `O(degree)` instead of
      `O(all objects)`, and the degree is in practice 1–2.
      **The anchor for it exists already:** the reason set from [Z24](#z24) knows per
      thread anyway **what it waits for** — today only as reason *class* (`IPC`), not as a pointer to the
      object. The extension from "which reason" to "which object" is the natural place;
      it replaces the search by the information that was available at blocking anyway.
      **Caution with the obvious version** (D8 pattern): whoever does not remove the list at waking
      as carefully as at entering swaps an expensive loop for
      a pointer to a dead object.

- [ ] **Kernel stacks lazy or shared** — 10,000 EL0 threads × 16 KiB = **160 MiB**, on a
      512 MiB machine **31 % merely for sleeping threads**; with kernel threads (64 KiB) it would be
      640 MiB and thereby impossible.

      **The water mark has existed since 2026-08-10 (`kstack` line, gates), and it answers
      the question AGAINST the expectation: 16 KiB are not generous, they are tight.**

      | Suite | Peak EL0 kstack | Reserve |
      |---|---|---|
      | `test-qemu-x86.sh` (without archive) | 832 / 16384 B (**5.0 %**) | 15552 B |
      | `test-qemu-x86-load.sh` (with archive) | **12008 / 16384 B (73.2 %)** | **4376 B** |

      **The deepest path is `SYS_LOAD`** — `loader::load_by_index` verifies an
      **Ed25519 signature and a SHA-2 hash in the kernel**, i.e. on the 16 KiB stack of the
      calling EL0 thread (largest frames of the image:
      `vartime_double_scalar_mul_basepoint` 3528 B, `NafLookupTable::from` 1928 B,
      `ed25519 verify` 1384 B, in addition `load_into_pd_mit` 1416 B). Record holder measured:
      thread slot 14, **program 1 = `init`**, the root task.

      **Thereby the original question is turned around.** "Can 16 KiB be lowered" meant so far
      "how much air is there"; the answer is **27 %**, and it hangs on a crypto library
      whose stack need nobody in this project has fixed. Three follow-up items, in this
      order:

      1. **The verification belongs off the caller's stack**, the stack not enlarged.
         A `SYS_LOAD` is the only syscall that needs 12 KiB — all others stay below
         1 KiB. As long as that remains, **every** one of the 10,000 threads pays for **one** path.
         If the verification moves to its own, core-local work stack (one per core
         instead of one per thread), 4 KiB per thread are plausible — that would be **40 MiB instead of
         160 MiB**, and the calculation above falls by a factor of 4.
      2. **A guard page is missing.** The kstack lies from `mem_alloc` without an unmapped neighbour;
         an overflow writes **silently** into foreign kernel memory. At 73 % utilisation of the
         deepest path that is no theoretical item. The same demand stands in
         [Z19/A4](#step-4--a3-stack-and-guard-page) for the **user** stack — it applies here
         just as well, and here an overflow hits the kernel.
      3. **Only afterwards lazy/shared.** To choose a smaller number without doing (1) would mean
         breaking the load path.

      **What the measurement does NOT reach** (it stands so in the report line too): the
      **syscall redirection path** (Z26/A3) — in both x86 suites no bound guest runs, the
      `redirect` chain is thus not contained in the water mark; **aarch64** (the counters run along
      there, the report line stands only on x86); and a **completely** used-up
      64 KiB kernel thread stack (it is recognised by the pattern, and without a pattern it drops out of
      recognition — for the EL0 class that does not apply, there the kstack is measured directly).

- [ ] **OPEN, from the same work: the `#DF` report cannot name the thread SLOT.**
      It stands in `SCHEDS`/`KSTACKS`, and both need a spinlock. A `#DF` can however have interrupted
      exactly the context that holds the lock — the handler would stand still and
      **no output** would come out, i.e. exactly the picture against which the message is built. What is identified
      is therefore the **stack** (lock-free from `TSS.rsp0`), not the thread. Fixable with a
      lock-free information about the running thread per core — `FP_OWNER[core]` (`system.rs`) is
      already an `AtomicU64` with exactly this information and merely has no public reader.
      **One line in `system.rs`, and the slot stands in the message.**

- [ ] **OPEN: `MAX_TSS_CORES = 16` is a static limit.** The TSS and its IST stacks must
      stand before there is an allocator (`gdt::init()` runs before `mmu::init_primary`). A core
      above it is **rejected and halted** (fail-closed, `ohne-TSS` counter in the report) instead of
      running without `RSP0` — on a machine with more than 16 cores that costs cores, not
      security. Fixable by the APs getting their IST stacks from the allocator (only the BSP
      needs static ones).

### C7. The capacity curve — where it REALLY breaks (measured 2026-08-10)

**Class:** measurement · **State:** **two of the three missing lines stand (2026-08-10,
afternoon) — C7 and [C4](#c4-efficiency-with-thousands-of-threads-eliminate-linear-scans) remain OPEN.** The curve said *that* it went up to
9984, but not *which stock tears next*. Since today it names the resource
(check line `mangel`) and shows the page-table pot over growing N (check line `ptab`) —
both gate, both with an isolating counter-proof. Still missing is the **stack water mark**.
**And the isolated row of the table below is to be re-measured**: its workers all faulted
at once (see below).

**The stack water mark has been delivered since 2026-08-10** (`kstack` line, gates in both
x86 suites) — and it turned the expectation around: not "16 KiB are generous", but
**12008 of 16384 B (73.2 %) on the load path**. Numbers, cause and the three follow-up items stand
at [C4](#c4-efficiency-with-thousands-of-threads-eliminate-linear-scans).

`tools/kapazitaet-messen.sh` creates PDs **with one thread each** (a PD without a thread is no
process) and prints the fill level over growing N. Target parameterisable via
`CAPROCK_SCALE_TARGET` (`kernel/build.rs` reports the variable as a build input — otherwise one measures
the predecessor state). The curve runs **at the very end** of `report_and_off`, after every verdict:
a measurement that occupies thousands of PDs otherwise tips every baseline-sensitive line of the run.

| Set-up | 512 MiB | 3 GiB | 6 GiB | Bound |
|---|---|---|---|---|
| SAS PD, **core-local** spawn | 4987 | — | **4987** | **Hosting capacity of ONE core** = `per_core × MIGRATION_HEADROOM` = 2500 × 2 = 5000. **Not RAM**: at 6 GiB 5771 MiB remained free |
| SAS PD, **load-balanced** spawn | 7212 | **9984** | **9984** | 512M: **RAM** (free RAM to 0, 64 KiB per process). 3G/6G: **thread slots** — 9984 + 16 already alive = **10,000 = `TARGET_THREADS`**, exactly the promise |
| **Isolated** PD *(outdated, see below)* | 224 | 1504 | 3040 | **RAM**, namely **2 MiB per process** (private region). Linear in the RAM size; free VSpaces at n=1000 still 4087, thus **not** VSpace/ASID |
| **Isolated** PD, worker in `.user_text` *(2026-08-10; the 3G number later no longer held, see below)* | **220** | **1477** | — | **RAM**, 2084 KiB per process (2 MiB region + **20 KiB page tables**). This row measures PDs whose thread **lives** — the one above did not |
| **Isolated** PD, 2 MiB region, state 2026-08-13 | **220** | **1119** | — | 512M: **RAM**. 3G: **NOT RAM**, but the fixed stock of split guard page tables (739 MiB free) — see [C7c](#c7c-the-stock-that-binds-now-16-split-guard-blocks-measured-2026-08-13) |
| **Isolated** PD, **64 KiB region** (C7b, 2026-08-13) | **321** | **979** | — | **both times the guard table pot** (419 resp. 2918 MiB free), **96 KiB per process**. And the threads demonstrably live: the curve counts the regions with a **trace** of their thread, not the table entries |

**The three statements that follow from it:**

1. **10,000 processes are reached — if they share an address space and from 3 GiB of RAM.**
   The bound is then the static thread capacity, and it is dimensioned exactly to the number.
2. **10,000 isolated tenants are not.** At 2 MiB per private region they would need
   **≈ 20 GiB** for that alone — and that is the case that the product goal is about. The
   capacity table (`NPDS = 10,000`) describes a **table**, no reachable
   tenant count.
3. **Whoever creates all processes from one thread hits 5000 and takes it for the
   system limit.** The difference between the first two rows is one line of code
   (`spawn_parked` against `spawn_balanced_parked`) and a factor of 2.

**Which row counts for the product goal — and why it is the worst.** The second row
occupies the **kernel data structures**; for a PaaS the **third** counts.

> **Correction of the own statement of the same day.** When this paragraph was first written,
> it relied on the row `224 / 1504 / 3040` — and that measured PDs **whose thread died
> at once** (`kurven_arbeiter` lay in `.text`). The conclusion survived the switch to living
> threads (220 instead of 224, 1477 instead of 1504; the order of magnitude is the same), **the
> origin of the number however did not**: it came from a series that did not measure what its name says. Whoever
> derives a product promise from a devalued row has been lucky, not right.
> The numbers below are those of the corrected series.

There the bound was RAM at **2084 KiB per PD** (2 MiB private region **+ 20 KiB
page tables**, measured separately), i.e. ≈ `RAM / 2 MiB` tenants — on a 64 GiB machine about **32,000**,
on the measurement machine three-digit. **"10,000 processes" is as a promise true only in the version that
is no tenant model.** That is how it belongs said, and not softer.

> **Second correction, 2026-08-13 (C7b).** The paragraph above computes `RAM / region size` —
> and thereby presupposes that **RAM** binds. Re-measured, that holds only at 512 MiB. Already at
> 3 GiB the series ended **before** RAM, namely at the fixed stock of split
> guard page tables (1119 PDs at **739 MiB free RAM**). The region size has since been
> lowered (2 MiB → 64 KiB, **96 KiB per PD measured**), and the density has **not** followed it:
> 512M 220 → **321**, 3G 1119 → **979**. Whoever derives a density promise from a cost reduction
> has not asked the next pot. The number that applies today stands in
> [C7c](#c7c-the-stock-that-binds-now-16-split-guard-blocks-measured-2026-08-13).

**The 2 MiB WERE lever number one — they are no longer, and that is measured.** The
classic answers (**lazy allocation**, **shared zero page with COW**) remain right and
remain territory of the **memory server** ([Z16](#z16-translate-source-text-instead-of-running-binaries--assessed-2026-08-09) stage 1); they move the density however
only when [C7c](#c7c-the-stock-that-binds-now-16-split-guard-blocks-measured-2026-08-13)
is fixed. The priority thereby turns around: **C7c before Z16 stage 1.**

- [x] **What do the 2 MiB consist of?** Broken down and measured — see
      [C7b](#c7b-the-2-mib-per-isolated-pd-are-measured-and-lowered--and-the-lever-was-the-wrong-one).
      Measured **before** the rework and **afterwards** once more, with the same kernel version and only
      the constant swapped; without the before-measurement at the same state the difference would have had to be attributed to
      half a dozen changes since 2026-08-10.

- [~] **The non-provoked reporting sites** (remainder of the above item). **Fell on 2026-08-11 from 21
      to 16: the sweep now drives the real LOAD PATH.** State (the summands add up
      against `system::MELDESTELLEN`, and a `const` assertion checks that at **build time** —
      prose had no gate, and exactly for that reason 11+9+10+1 = **31** stood here before next to
      a denominator of 32):
      * **16 provoked** — 12 in **both** suites, 4 only with a boot archive (`4288` segment memory,
        `4304`/`4366` page tables, `4350` user stack). What is run is `system::load_into_pd_mit`,
        i.e. the same function as with `SYS_LOAD`, with a **stop rule instead of a guessed
        cap**: one pass after the stack pot is the end, because the next would START
        a foreign program without any cap.
      * **A finding on the side:** `map_region_into_thread` (4608) stood booked as "load path" and
        is none — it is the way to the **device window** of a driver PD and needs no archive.
        It has since been provoked in **both** suites. A reason that nobody re-checks
        survives every environment.
      * **10 place pots** (thread slot, ASID, colour stripe, PD slot; +1 compared with the old
        count, because `4171` colour stripe wrongly ran under "load path"): the lock sits
        in the memory allocator and does not hit them. A second lock on the place pots would be
        cheap — the question is where it belongs, without the capacity curve dragging it along.
      * **3 "the allocator was not asked"** (`MANGEL_MAPPING_ABGEWIESEN`): reachable via
        an image with a **crooked segment VA** — that is exactly how the `wasm` load error of 2026-08-10
        arose. An image of this kind in `tools/mkarchive.py` would be the next cheap stage.
      * **1 fixed HAL stock** (`MANGEL_GUARD_TABELLE`): the HAL has no allocator.
      * **2 structurally not triggerable, and that is a finding about the sites themselves**:
        `4207` (`vspace_l2` returns for an ASID just delivered by `create_vspace_masked`
        **always** `Some` — dead branch) and `4269` (fail-closed against more than `MAX_IMG_SEGS`
        frame pieces: that needs an IMAGE, no empty pot).

- [ ] **Still without a fill-level display** (remainder of the above item): the **FP slab**, the
      **VSpace/ASID table** (only as `free_vspaces()` in the curve, not in the report), the
      **IRTE table** and the **finalisation buffer**.

- [ ] **The third row continues to be missing: the stack water mark** (see [C4](#c4-efficiency-with-thousands-of-threads-eliminate-linear-scans),
      "Kernel stacks lazy or shared"). Measured is how **large** the stacks are — not whether
      they **suffice**.

### C5. GIC scaling (ARM blocker for > 8 cores) — **continues open**

`caprock-hal::gic` is **GICv2** (8-CPU limit, the `GICD_SGIR` target mask is 8 bit). More than 8 cores
need on ARM **GICv3/GICv4** (redistributors per core, `ICC_SGI1R_EL1`, ITS for MSI). QEMU:
`-machine virt,gic-version=3` (up to 512 vCPUs). **As long as that is missing, >8 cores are not
testable on ARM**, independent of the capacities. The capacity side has been prepared since ext-30
(`MAX_CORES = 256`, tables dimensioned at boot time) — what is missing is the interrupt hardware.

### C6. x86-64 port — remainder

`caprock-hal` is now architecture-selective (`src/aarch64/` + `src/x86_64/` behind the same API).
The **kernel core runs on x86_64**: the same self-tests, the same preemptive scheduler,
the same cap-guarded IPC — without a single `cfg(target_arch)` in the core. Details:
`README-X86.md`.

- [ ] **PCID** as an optimisation: the address-space switch currently flushes the whole TLB (the ASID is
      a pure software identifier). With `CR4.PCIDE` + tagged entries that would drop out

- [~] **IOMMU (VT-d)**: steps 1–3 done (see done.md) — `attach` returns `Some`,
      `dmawin`/`dmatok` run on x86 with the **same** function as on ARM.
      **Open:** step 4 (interrupt remapping incl. CFI switch-off, presupposes queued invalidation)
      and the multi-unit aggregation (AGAW minimum, counters over all DRHDs).

- [ ] **Boot archive** via Multiboot modules (`SYS_LOAD` currently fails cleanly)

- [~] The ARM-side demo/test services (`threads/mod.rs`, 5402 lines) are strongly ARM-coupled
      (EL0 isolation, MMIO/RTC, DMA). `dmawin`/`dmatok` have been pulled out into `dmatests.rs` and
      run on both architectures; **open** are `dmagen` (needs an arch-neutral
      leaf read-back; the coherence sub-check does not exist at all on x86 — legitimate SKIP) and
      the virtio negative test (hangs on the ARM virtio driver). Connected with [F1](#f-take-debugtest-code-out-of-the-release-build):
      the same module, two reasons to split it.

---

## E0. The x86 DMA core is built — and the counter-proof found two holes (2026-08-17)

**Class:** done with remainders · **State:** `iohealth` green on **both** suites, host tests green

**What was built: ONE statement instead of two formulations.** Until here every
architecture reported the health of its IOMMU in its own words — aarch64 as `smmu` (IDR0, SIDSIZE,
SMMUEN, CMD_SYNC round trip, event queue, GERROR), x86 as `iommu`/`vtdcaps`/`qi`/`ir`. Both
implementations were complete; **there was only no sentence that means the same on both sides** —
exactly the warning signal that the VT-d entry below named in advance
(*"a separate `vtdtest` would mean that the properties are formulated differently on x86"*).

New: `crates/caprock-hal/src/iommu_health.rs` — **pure type over fed-in values**, without
register access, therefore host-testable as a file (pattern `dmar.rs`/`irte.rs`), 9 tests, in the target
`iohealth` of `tools/host-tests.sh`. Both facades deliver `health(round_trip)`, both
boot-up paths print the same line, both suites check it **by name**.

**The load-bearing rule stands in the type, not in the comment:** `faults_empty` is weighed only once
`invalidation_round_trip` is proved. A dead unit likewise reports an empty queue —
the same form as the empty event queue without `CD.R`. A host test checks exactly this order
(`the_undermining_reason_wins`).

- [ ] **FINDING 1, and it is the actual yield: `FSTS.IQE`/`ICE`/`ITE` has been read by NOBODY.**
      `REG_FSTS` occurred in the whole x86 tree only with `PFO` (bit 0) and `PPF` (bit 1). The three
      invalidation error bits are **sticky (RW1C)**, and as long as `IQE` stands, **the
      unit does not process the invalidation queue any further** (VT-d 6.5.2.9) — `IQH` stands still.
      Afterwards *every* `qi_submit` runs into its poll bound and returns `false`.
      **Failure picture: "the invalidation fails". Cause: a bit from earlier. Nothing in the tree
      named it.** Now read by `vtd::fault_status()` and gated in the `hw_error` field.
      **Open:** they are read, but **cleared nowhere**. A unit that has set `IQE` once
      remains jammed for the rest of the run — the line now says it, but does not fix it. That is the
      next work here.

- [ ] **FINDING 2: the aarch64 suite did not read the new line — found by the counter-proof.**
      Under the mutation the kernel gated correctly (`bringup : offen: iohealth`, watchdog
      fired, **exactly one** open conjunct) — and `test-qemu.sh` nevertheless reported
      `== ALL PASS ==`. The x86 suite caught it via its generic NEW-RED detection, the
      aarch64 suite has none. **A line that no suite reads gates nothing** — the same
      form as `root` before D5. Fixed: both suites now check `iohealth` **by name**, and
      by name instead of generically, because a NEW-RED detection catches a red line, but not
      its **disappearance**.

- [ ] **Two own errors in the build, both from the trap register.**
      **(a)** The first version of `health()` called `invalidate_context_cache()` **inline** — a
      health query would have triggered a real global invalidation on every call.
      *A reading that changes its subject is no observation.* The round trip is
      now passed in, on both architectures with the same signature.
      **(b)** "Both targets build" was measured on the **wrong configuration**: `threads/mod.rs`
      lies behind `--features selftest`, my build ran without. The error (`DONE_FLAGS_ARM` 57 → 58)
      appeared only when the suite built. *"`cargo build` runs through" is no evidence as long as
      nobody binds the configuration.*

- [ ] **What `TODO.md` kept outdated at this place** (looked up instead of remembered): step 3b
      ("queued invalidation is a precondition of 6.4") and step 4 (interrupt remapping) stand
      below as `[ ]` — both are **built**: `GCMD_QIE`, `QI_IEC_INV`, `qi_submit` with
      wait descriptor, `GCMD_IRE`, `cfi_blocked()`, and the suite checks `qi` and `ir` by name.
      The entries belong in `done.md`. *A register that keeps finished items makes the question
      "what is open" unanswerable.*

---

## E. DMA hardening — remainder

Order by structural effect, not by effort.

- [ ] **Core hand-over to Caprock (variant B) — stage 2.** Stages 0/1a/1b are proved:
      `tools/handover/` takes five E-cores offline, sends them INIT-SIPI-SIPI into a
      trampoline and brings them into long mode with their own GDT, their own page tables and
      their own CR3 — repeatable, without reboot, `rmmod` gives everything back.
      Done on the kernel side:
      * **x2APIC** (MSR path). Decision for maximum performance *and* because xAPIC can address only 255
        cores. Two-stage transition off -> xAPIC -> x2APIC.
      * **Memory as a range list** (`init_mem_regions`), always run fragmented in the QEMU run.
      * **`HandoverInfo`** as a common structure of both start paths, also used and
        checked by the Multiboot path.
      Open, in this order:
      1. **Image placement without relocation.** The kernel is linked at 1 MiB, and there lies
         Linux. Instead of making it position-independent: the module puts the image into a
         4 MiB block (`alloc_pages(order=10)` — exactly the buddy upper limit, the image is 3.1 MB)
         and maps in **its** page tables VA 1 MiB onto this PA. `mmu::init_primary`
         must then adopt the mapping instead of pure identity — an offset from the
         `HandoverInfo`, no relocation.
      2. **Console as a ring** instead of UART: the serial interface belongs to the host, two
         writers yield interleaved lines. Ring in the `HandoverInfo`, readable by the module via
         debugfs.
      3. **What must stay out on handed-over cores:** VT-d (Linux owns the units for
         interrupt remapping — `GCMD.TE` kills every Linux DMA) and `intc::init_dist()` (puts
         the 8259 to rest, that is global).
      4. **ELF loading in the module** and the jump into the image.
      If a taken-over core runs real kernel code, `release=1` on unloading is no
      longer self-evident — then `release=0` and reboot.

- [ ] **VT-d allocation (item 6)** — acceptance criterion **in advance**: not "new x86 tests green",
      but **`dmaalign`/`dmawin`/`dmagen`/`dmatok`/audit 4–7/negative test stop
      skipping** — without x86 special paths in the tests. A separate `vtdtest` would be the
      warning signal: it would mean that the properties are formulated differently on x86, and then
      a second design exists after all. What still skips afterwards is the honest list of what
      x86 does not have.
      Decomposition:
      1. [x] Read **`VtdCaps`** once at boot-up, log, derive every condition from it
         (not decide at the point of use). In addition the order trap
         fixed: `GCMD` is no RMW register, and `TE = 0` means **free DMA**, not
         blockade — every command now goes via `gcmd_issue`, which takes the state bits
         from `GSTS`. An `SRTP` write "only with the command bit" would have switched the translation
         off for the duration of the switch, and the test would have read it as success.
         **Measurement on the target platform (QEMU q35 + intel-iommu):** SAGAW `0x6` (39 **and** 48
         bit) → 39 bit / 3 levels chosen, so that the window arithmetic does not exist twice;
         MGAW 48 → input limit `0x80_0000_0000`, identical to the ARM side; ND → 65536 domains;
         QI and IR present; scalable mode off; **one** fault recording register.
         Three values that entail work:
         * **`CM = false`** — different from expected. QEMU's `intel-iommu` has `caching-mode` by
           default **off**, which means: the set-up forgives here a missing "invalidate after creating",
           and on `CM = 1` hardware it would break. The direction of the divergence is thus reversed to
           the expectation, the consequence the same: invalidate unconditionally, only log `CM`.
           For step 3 in addition `caching-mode=on` in the test set-up, to enforce exactly that.
         * **`ECAP.C = false`** — the unit is **not** page-walk coherent. Writes
           to root/context/second-level entries need a cache clean. Concerns the
           tables, not the buffers — a path that `dma_granule()` does not cover at all, and that
           QEMU does not punish.
         * **`SC = false`** — no snoop control, no-snoop cannot be overridden. "x86 is
           coherent" thereby holds only as long as no device uses no-snoop. Belongs as a condition
           at `dma_granule() == 1`, not as a constant.
      2. [x] DMAR/DRHD incl. device scope, **group formation from ACS**, RMRR exclusions.
         Implemented as a **pure function over fed-in data** (`hal::dmar`, `forbid(unsafe)`):
         `parse` takes a byte slice, `build_groups` a topology description. The real path
         fills both from ACPI/PCI enumeration, the self-test from literals — on the
         standard set-up (flat, no RMRR) the exclusion path and group cases would otherwise **never** run.
         Sensitivity proved: "catch-all in the same loop" turns `catch_all_last` **and**
         `bridge_scope_subtree` red, "type 2 like type 1" only the latter — both times exactly the
         assurance that the mutation violates.
         Real measurement (QEMU q35): 7 devices, 1 unit, **5 groups**, 0 exclusions, no ATSR,
         segment throughout 0, oracle 0.
         Open from this step: several DRHD units are **parsed**, but `VtdCaps` is
         still one unit — the minimum over all units that scope an allocatable group,
         and the aggregation of fault/config counters over all units are pending (until then
         the oracle would be blind to everything that does not hang on unit 0). A group that scatters across
         units is already excluded **completely** (`GroupSpansUnits`).
         Old: output:
         List of allocatable **groups** (not devices — without ACS on all upstream bridges
         the isolation granularity is the group: peer-to-peer behind a switch bypasses
         the IOMMU, multi-function devices without ACS share the RID view). The naming `dma_group`
         from the beginning, not retroactively — otherwise tests hang on the too strong statement.
         RMRR-afflicted devices are **rejected and logged**, not allocated with a gap:
         part of their access lies by construction outside the control.
      3. [x] Root/context tables + SLPT, `attach` returns `Some`. **Milestone reached.**
         Finding up front that concerned the criterion: the DMA tests on x86 did not *skip* —
         `threads/mod.rs` is `#[cfg(target_arch = "aarch64")]`, they did **not exist at all** there.
         A test that does not exist on one architecture cannot turn green there either.
         `dmawin`/`dmatok` therefore now lie arch-neutral in `kernel/src/dmatests.rs`, and
         both bring-up paths call **the same function** — not a rebuilt one. Both on
         x86 green, aarch64 unchanged.
         Implemented: SLPT (3 levels, 39 bit), context entries for **all** RIDs of the group (they
         end up in `DmaCtx::sids` and thereby run through the same `ctx_quiesce` loop as on
         ARM — the trap from `13810e9` is structurally avoided), DID from 1 (with `CM=1` 0 is
         reserved), `FPD` stays off, context cache **before** IOTLB, unconditionally also after
         creating. Direction drops out instead of being added: presence *is* `R|W`, `ToDevice`
         becomes "only R". `SNP` is not set without `ECAP.SC` (reserved). Every
         table write goes through `write_entry`, which without `ECAP.C` executes `clflush`+`sfence`
         and never returns without a flush — also on the intermediate levels.
         Context tables arise **once at bring-up** (256 x 4 KiB), not at the first
         `attach`: they are a bus structure, must not be freed when a context is torn down,
         and a lazily created, never freed table would look like a leak in every
         resource balance. Symmetric to the linear stream table on ARM, and
         the DMA path thereby gets by without allocation.
         Side finding in the test itself: the 32-bit assumption in `dmawin` was **machine**-,
         not architecture-dependent — on the x86 set-up with 512 MiB of RAM the window lies below 4 GiB,
         and the test would have been green without checking the property. Now 20 bit.
         Open from this step: `dmagen` and the virtio negative test still hang on the
         ARM module (leaf read-back resp. virtio driver); QI is deliberately left unused, the
         kernel runs exclusively the register path.
         Old:
         In doing so: **`FPD`** (fault processing disable) in the context entry is literally `CD.R` once
         more — set, the negative test would again pass on a structurally empty observation.
         And `FSTS.PFO` (overflow of the fault recording registers) must go into the same
         counter as `config_errors()`, otherwise "no further faults" after a storm means
         nothing. (`fault_overflow()` already stands.)
      3b.[ ] **Queued invalidation is a precondition of 6.4, not an alternative.** The
         invalidation of the interrupt entry cache exists only as a QI descriptor, not as
         a register operation — IR cannot be operated correctly without QI. The register path is
         thereby a **makeshift with a known expiry date**: build nothing further on it,
         and on switching over really switch instead of keeping both.
      4. [ ] Interrupt remapping (`GCMD.IRE`) incl. **switching off the compatibility-format
         interrupt** — IR active with CFI still permitted is an open door at the side.
         Own line in `docs/invariants.md` §2, because it has the same structure as the
         BME/STE division of labour: two mechanisms of which neither replaces the other.

---

## D15. The kernel jumps to address 0 — 2 of 600 aarch64 runs (2026-08-08)
**Class:** error · **Effort:** open, cause unknown · **Site of discovery:** aarch64 measurement series after
the audit correction

- [ ] **The picture, twice of equal form:**

          el0-trap: User-Thread 0x100000426 faultete (EC=0x20 FAR=0x0) -> beendet, Kernel laeuft weiter

          [EXCEPTION] unerwarteter Trap
            kind=4 (Current EL SPx)
            ESR=0x000000008600000d (EC=0x21)
            ELR=0x0000000000000000
            FAR=0x0000000000000000

      `EC=0x20` at the user thread is an **instruction abort from a lower EL** with
      `FAR=0` — its PC stood at 0. Afterwards the **kernel** takes `EC=0x21` (instruction abort at
      the same EL) with `ELR=0`: it itself jumped to 0. The run ends there; the external
      time limit clears it away (`rc=137`).

      For comparison: the *deliberate* isolation probes fault in the same series with
      `EC=0x24 FAR=0x40000000` (data access). `EC=0x20 FAR=0` is a different animal.

- [ ] **The rate: 6 in 2000 runs = 0.30 %** (95 % interval about [0.11 %, 0.65 %]), measured on
      2026-08-08 at a fixed parallelism of 6. Of 63 deviations of the same series 57 are D13
      (`offen: color`) and **0** an audit finding — the code-7 correction carries over 2000 runs
      (before 1 in 600).

      The first observation (2 in 600) was too small for a rate; only this series gives one.

- [ ] **The before series — and the discriminating power, calculated BEFORE the start.** If D15 arose through
      the rework, the before rate is 0. How large must the series be for a null finding to
      mean something?

      | Before series | `P(0 hits, if unchanged)` | `P(all 6 in the after series)` |
      |---|---|---|
      | 600 | 0.165 | **0.207** — does not carry |
      | 1000 | 0.050 | **0.088** — does not carry |
      | **2000** | 0.0025 | **0.0156** — carries |
      | 3000 | 0.0001 | 0.0041 |

      So **2000**, not 600. The aarch64 bisect of 2026-08-04 omitted exactly this step
      (0/6 against 1/18, Fisher p ≈ 1) — and that stood fixed afterwards instead of beforehand.

      Running since 2026-08-08 in a worktree on `2ef9ddb` (state before the D0 rework), with the
      **new** measurement rig: that is measurement infrastructure, not the object of checking.

- [ ] **And the statement that had led to this was a null finding without a size.** I wrote that the
      picture had "appeared in **no** aarch64 log before the D0 rework". That was true for the
      logs that I had — a handful of `RUNS=6` runs. At 0.15 % the expected
      number of hits in them is **0.02**. "Never seen" was thus the most probable observation, no matter
      whether the error was there.

      Exactly the same fallacy as with D0 on 2026-08-03 (2300 clean runs → "excluded",
      actually 66 % chance of a null finding). It happened here a second time, in
      the same session in which it was written down as a lesson — this time in the form
      **"I have never seen it"**, which names no sample size and therefore slips through
      even more easily.

- [ ] **The trace that remains: both threads lie on REUSED slots.**
      `0x100000424` and `0x100000426` — generation **1**, slots 1060 and 1062. The `scale` test
      creates 1024 threads at the same time and tears them down; generation 1 means the slot has already been
      recycled once. A slot that is recycled while somebody still points at it
      would yield exactly an `sp`/`entry` of 0.

      That continues to hold independent of the D0 rework and is the next point of attack — the suspicion
      is now directed at `reap`/`record_zombie`/`alloc_tcb` **as such**, not at their
      interplay with parking.

- [ ] **What is to be done next, in this order:**
      1. ~~A series that yields a rate~~ — done: 0.225 % pooled over 4000 runs.
      2. ~~The same series on the state BEFORE the rework~~ — done, see above: the rework is not it.
      3. **A reporter in the reap path** that counts the *opportunity* instead of the hit. At
         0.225 % a reporter that speaks only on misfortune is mute in 444 of 445 runs.
         To count would be: is a TCB slot recycled to which a reference still points (directory,
         ready queue, `sc_donee`, reply token)? The generation is there for that — an access with
         a stale generation is the opportunity, and it is countable in every run.
      4. **Record the entry point where it applies.** Write `entry`/`sp` of a thread along at
         `alloc_tcb` and print them at the EL0 fault with `FAR=0` — then the log says
         whether the thread was created with `entry=0` or got there on the way.
         That is the difference between "set up wrongly" and "overwritten".


**Class:** evidence gap · **Effort:** one measurement, one Verus model

- [ ] **The acceptance measurement is not comparable with the discovery measurement.** Between the two the
      memory controller was corrected (RSS at the subshell instead of at QEMU's process tree, 192 instead of
      361 MiB per run) — the parallelism was thus a different one. With a **start race** exactly
      the load is the quantity that produces the hit rate: `P(0 | unchanged) ≈ 1.2·10⁻⁴` thus stands
      for "fixed **or** less pressure", and the two are not separated.

      **And the condition of the discovery measurement can no longer be established** — its log is cut off in the
      middle, the line "controller stands at N workers" is missing.

      **To do:** run the fixed kernel under a *fixed* worker count, at least as high as
      the discovery measurement could have (its RAM budget allowed 42, CPU bound at ~16). If at
      **fixed 24** nothing is hit, "less pressure" is excluded — the condition is then
      monotonic in the direction that counts. `ARBEITER_FEST=24 tools/d0-messen.sh 50000`.
      The summary has named the condition itself since 2026-08-07.

- [ ] **The x86 series almost does not check the rework.** `pdbind` counts **3** bindings on x86, on
      aarch64 **70** (`kernel/src/threads/mod.rs` is `#[cfg(target_arch = "aarch64")]`). 50,000
      x86 runs cover three admission sites. The measurement rig has run `ARCH=arm` since 2026-08-07;
      a series of the order of 2000 is worth more there than further x86 runs.

- [ ] **Verus says nothing about D0.** The IPC model does not know the concept "thread without PD" — zero
      occurrences of PD binding or `ERR_NOPD` in `Verification/ipc/proofs/`. "16 files, 0
      errors" here means only that the existing proofs continue to hold.

      What was missing is an **invariant** that ties `RECV` to a bound PD — mere
      representability of the state proves nothing. That is no small addition: the model
      knows today only endpoints and queues, no PDs; to introduce a `pd_bound` predicate
      means to extend the model state and to redo the existing proofs over it.

- [ ] **A dropped `Parked` is no compile error.** `#[must_use]` makes it a
      warning; a `let _ = spawn_parked(..)` swallows it. The type covers the more dangerous case
      (a `ThreadId` that escapes before admission), not this one. A `Drop` impl would be **no**
      way out — with it the field in `admit` could no longer be moved out, and the type would lose
      exactly the property that is at stake.

## C4 addendum: the 40 MiB are REDEEMED, no longer forecast (2026-08-12)

`USER_KSTACK_SIZE` stands **on x86 at 4 KiB** instead of 16. Lowered in two stages, each stage proved with both
suites:

| Size | Peak (load suite) | Reserve | Suites |
|---|---|---|---|
| 16 KiB, **before** C8 | 11,992 B — **73.1 %** | 4392 B | ALL PASS |
| 16 KiB, after C8 | 1,312 B — 8.0 % | 15,072 B | ALL PASS |
| 8 KiB | 1,312 B — 16.0 % | 6,880 B | ALL PASS |
| **4 KiB** | **1,312 B — 32.0 %** | **2,784 B** | ALL PASS, acceptance series 12/12 |

**10,000 EL0 threads thereby cost 40 MiB instead of 160** — on a 512 MiB machine 7.8 % instead of 31 %.

**The justification hangs on three things that did not exist before**, and without them the number would be
guessed: the **guard page** (an overflow is a `#PF`, no silent hit), the **IST stacks**
for `#DF`/`NMI`/`#MC` — an NMI does not ask `IF` and **no longer** runs on this stack —, and
the **water mark**, which re-measures the value in every run instead of believing it.

**On aarch64 it stays at 16 KiB.** There all three are missing. A number that is measured on one architecture
and taken over on the other is guessed on the other — `MASK_BITS` was coincidentally right on
x86.

- [ ] **Bring aarch64 up to date** — guard page (block splitting), an IST counterpart and the
      water mark line. Only then is the constant there more than a precaution, and
      only then does `unbewachte_stacks()` no longer count every stack.
- [ ] **What the 4 KiB do NOT cover:** the syscall redirection path (Z26/A3) runs in neither
      of the two x86 suites; its contribution to the water mark is unmeasured. Whoever wires it up
      re-measures the line before believing it.

## C8. The VERIFIER THREAD — **built** (2026-08-11); only (c) remains open

**Class:** capacity / security · **State:** (a) and (b) are **built and measured**, the
derivation including counter-proofs stands in done.md. What remains open stays here.

**What is done, in one line:** `SYS_LOAD` no longer verifies on the 16 KiB kstack of the
caller, but on the 64 KiB stack of a dedicated verifier thread. The caller
blocks regularly with its own reason `BlockReasons::LOAD` (Z24); the overflow of the job queue
is called `ERR_LOAD_BUSY` and leaves the overflower **runnable**. Measured (load suite, 512M):
EL0 kstack **11,992 → 1,312 B**, verifier stack **12,328 / 65,536 B**.

- [ ] **(c) The "crypto out of the kernel" question is a design OF ITS OWN, no subordinate clause.**
      Signature checking is **root trust**. It can move into a PD — but then
      *this PD* is TCB, and the gain is real only if the kernel trusts the **result** via a
      **narrower channel** than the code itself (e.g.: the kernel only checks a hash
      against a release signed by the verifier). For now the core-local thread suffices;
      the PD version stands here as a **named option**, so that it does not vanish as "we once
      considered it".

- [ ] **Left over from the build: `AUFTRAEGE_MAX = 4` is a chosen number, not a
      derived one.** It stands at one place and in the report, but it is policy: it says how
      long the DoS channel may become before rejection. What is missing is a **budget per PD** —
      today a single PD can occupy all four places and delay foreign loads by exactly as much
      as a load takes. The rejection is thus named, the **fairness** is not.

- [ ] **And: `heimatkern` is a statement, no evidence.** The job carries the core of the caller
      along, so that the placement remains the same after the move — that is checked only indirectly
      (`ladepol` reads affinity back from the TCB, but only for programs that *have* one in
      the manifest). A program **without** affinity has today no line that reads its core back.

- [ ] **NEW and serious: `caller_pd` has been A NUMBER FROM THE PAST since C8.**
      Before C8 `load_by_index` ran synchronously in the caller's syscall — the caller PD was
      by construction alive. Now between the hand-over and the execution there lies a
      scheduling window, and `caller_pd` is an **index**. If the caller dies in this window
      and its PD slot is reallocated, `create_hardware_backend(caller_pd, ..)` binds the
      driver backend to the **wrong partner** — and the cap check would run through correctly,
      exactly as with the device window in the shared page directories.
      **Not triggered today** (the only `SYS_LOAD` caller of the suites is the root task, and
      it does not die), but that is a statement about the test cases, not about the code.
      The structural version is the same as with `ThreadId`: the job would have to carry a
      **generation-bearing** PD reference instead of an index, or the execution would have to re-check the
      identity of the caller before use. A half fix ("is the caller still
      alive?" immediately before loading) merely shifts the window and looks like a fix.

- [ ] **The speak check `laeuft` means "was started", not "lives".** It reads
      `verifizierer::thread_id().is_some()`, i.e. a field that `starten()` sets once — not the
      thread directory. Today both coincide, for a nameable reason: the verifier
      has **no PD** and there is **no tcb cap** on it, so it cannot be killed from ring 3. If a way ever arises to
      end it, the line becomes **silently false** — it would report "laeuft" while `uebergeben` puts jobs into a queue that nobody empties any more
      (until `ERR_LOAD_BUSY`). The fix is one line (`caprock_sched::is_live`); it stands here
      and not in the code, because it is **not falsifiable** today — a guard that no
      counter-proof can reach is a claim.

- [ ] **Smaller, but from the same root: a load result can go into the void.** If the
      caller dies while its job waits, the PD is loaded nevertheless and nobody learns its id.
      That is counted (`Antwort-ins-Leere` in the `verif` line, today 0), not cleaned up.
      To be decided is whether such a job belongs **discarded** at the caller's death
      — then the dying path needs a grip into the job queue, and that is the place at
      which the lock order must be checked once more.

## D13. The suite has checks that measure in WALL-CLOCK TIME — and the measurement rig is overbooked
**Class:** measurement rig · **Effort:** small, but the delimitation is the actual work

- [ ] **Measured on 2026-08-07: 4 deviations in 50,000 runs, all four load artefacts.** The
      D0 measurement rig runs 16 guests of 4 vCPUs each on 20 cores — **3.2-fold overbooking**. If a
      vCPU is preempted by the host, ticks and TSC continue to run, the guest execution does not.

      | Picture | Number | Measured value |
      |---|---|---|
      | `cycles : FAILURES` | 2 | 1 ms window = 11,689,334 resp. 11,720,490 cycles instead of 2,803,578 — **factor 4.2** |
      | `freeze : FAILURES` | 2 | a ~30 ms window saw **0** worker rounds instead of 3 |

      Evidence: `docs/befunde/d0/lastartefakt-{freeze,cycles}-2026-08-07.log`.

- [ ] **How to tell it from a kernel error — by the FORM, not by the number.** In
      one of the two `freeze` failures `laeuft-vorher=false (49->49)` failed: the
      **positive control**, measured *before* anything is frozen. An error in the thaw path cannot
      structurally cause it. The obvious reading ("the D0 conversion damaged `thaw`")
      was thereby refuted, without looking at the thaw path at all.

      That is the general rule for this measurement rig: **on a deviation first ask
      which conjuncts fall — not how often it falls.** A counter alone would have led here to
      a bug hunt in the scheduler.

- [ ] **What is to be done.** The affected windows are defined in wall-clock time (`hal::timer::ticks`
      resp. a 1 ms calibration). Two ways, and the first is the better:

      1. **Count in the measured quantity instead of in time.** For `freeze` that means: wait until
         the counter has moved by N, with an upper bound — then "it stands" is the statement,
         and not "it did not move in 30 ms". The same consideration as with D10, where
         an iteration count replaced a stopwatch: *an iteration count is a property of the
         program, a time measurement is not.*
      2. Lengthen the window. Cheaper, but it merely shifts the limit — at 6-fold
         overbooking it falls again.

      **Not**: hang the check out under load. A test that is silent under load is silent exactly
      when it is needed.

- [ ] **The rate on aarch64: 57 in 2000 runs = 2.85 %** at fixed parallelism 6 (measured
      2026-08-08) — by far the most frequent deviation of this series, and it is an artefact
      of the measurement rig, no kernel finding.

- [ ] **On aarch64 it is the same, and there it is the MOST FREQUENT cause** (measured
      2026-08-07): 32 runs at 16-fold parallelism, **9 deviations, all nine
      `bringup : offen: color`**. The colour lines are byte-identical to the reference (`color`/`stripe`
      ALL PASS, `pprobe` SKIP) — nothing fails. The watchdog fires **between** the printing
      of the colour suite and the `COLOR_DONE` store: the deadline is 6000 **ticks**, and the colour suite
      is gated on `cross`/`strand`/`loadstop`, thus runs last.

      Thereby an assignment is also corrected: the aarch64 hang at the D0 rework was **not** D6
      and **no parked thread**. The question became answerable only because the
      aarch64 watchdog has since 2026-08-07 **named** what was open.

- [ ] **Caveat on the number.** 2 `freeze` artefacts in 50,000 against 0 in the 56,895 runs before is
      **not** significant (Fisher p ≈ 0.2). So there is no evidence that the sensitivity is
      new — only that it exists.

## D. Verification

- [ ] **NO CI RUNNER — all gates wait (2026-08-03). This is the remainder of a finding whose
      first two layers are closed today.** In order, because each layer concealed
      the next:

      | # | Finding | State |
      |---|---|---|
      | 1 | Gates lay in `.gitea/workflows/`, the server is **GitLab** — it does not read that | **fixed**: `.gitlab-ci.yml` |
      | 2 | `.gitea/workflows/kani.yml` had been **invalid YAML** since creation (`: ` in the job name, l. 63 col. 49) | **fixed** + `tools/ci-yaml.sh` with self-test |
      | 3 | **no runner registered** | **open — here** |

      Measured, not conjectured: pipeline 5 stood **284 s** in the queue, pipeline 6
      **939 s (16 min)** — both with all jobs `pending`, `runner=None`, `tag_list` empty. A
      registered runner picks up within seconds; 16 minutes are no slowness any more. The runner API (`/projects/2/runners`) needs a token that is not available here —
      **which** runner is missing (no runner / wrong tags / `paused`) is thus not yet
      told apart.

      Before the push there existed **two** pipelines in the whole project history, both of
      2026-05-23, both on `main`, both with GitLab's Auto DevOps template
      (`semgrep-sast`, `container_scanning`, `code_quality`, `build`, `test`) — never with our
      gates.

      **Why this is more than a configuration task.** The Verus proof in `cap_space.rs`
      was red for 37 days, while the associated commit was called "delete (leaf) proved against the FULL
      cap_inv" and the README claimed "10 verified". The obvious explanation was
      "ran, but nobody looked". It is false: **there was nothing to see.**
      A gate that stands on no server is no weaker assurance, but none —
      and it cannot be told from a green gate from outside. That is the same form
      as the empty event queue without `CD.R`.

      **Acceptance condition — and it is deliberately not "the CI is green":** be able to show
      a pipeline ID whose `verus` job has **run**. In addition three cases that
      belong told apart, because they look the same from outside:
      (a) no pipeline → configuration not found;
      (b) pipeline with `pending` jobs → no runner (the case today);
      (c) jobs with a result → only here does green say something.

      Side item, as soon as a runner runs: the `verus` job runs `--selftest` **before** the
      actual run. If the self-test is green, but the main run also — only then is it
      proved that the gate *can* fail and nevertheless does not fail.

      Own trap in the process, for the next who measures: **every push cancels the previous pipeline
      of the same branch.** Pipelines 4 and 5 went to `canceled` that way — that looked like "runner
      refused" and was my own next commit.

- [ ] **Continue the cycle counter** (stage 1 partly done): `hal::timer::cycles()` /
      `cycles_per_sec()` / `invariant_tsc()` stand on both architectures, serialising and
      calibrated against the PIT. Open: the per-TCB accounting (`consumed_cycles`, stamped on
      switching in/out) and on it the monitoring cap.
      **Invariance is telemetry, no pass condition** — but the restriction hung on the
      emulator, not on the design: under KVM with `-cpu host,+invtsc` the same check reports
      `true` (`+invtsc` must be requested **explicitly**, QEMU leaves it out even with `-cpu host`,
      because it blocks live migration). Zen/EPYC has invariant TSC throughout. The
      TCG fallback remains and says in the log that the values there are indicative — and that the
      promise exists elsewhere.

- [ ] **RAM size as a test parameter** (partly done): `test-qemu-x86.sh` takes `-m` as
      second parameter (512M/8G checked). For aarch64 the same is still pending.

- [ ] **Count bounds + lock section as operation count** (next step, see discussion):
      iterations per thread death in `purge_ipc_queues`, CDT walk length, `revoke` subtree size,
      table occupancy, stack bytes per thread — as a **count**, so that it is machine-independent. The
      Amdahl calculation then decomposes into "operations under lock per thread life cycle" (now
      available) times "cost per operation" (calibrated on bare metal). With KVM the direct
      section measurement is additionally possible (resolution 51 cycles), so both.

- [ ] **D1** *(core question answered, the rest is a design decision)* Kani covers the
      ext-29 property of `caprock-sync` **structurally not** — proved 2026-08-01: Kani
      builds for the host target, there the third `cfg` branch applies with
      `IRQ_MASKING_IMPLEMENTED = false` and `irq_save_disable()` as a no-op; the run says it
      itself (`warning: constant IRQ_MASKING_IMPLEMENTED is never used`). A green `sync`
      proves memory safety and arithmetic, not IRQ safety.
      **Compensated:** the property is carried by the guard at compile time, and that it
      *can* trigger is now checked instead of claimed — `tools/guard-verify.sh` builds
      unchanged (must compile) and with destroyed `cfg` branches (must abort at the guard),
      for both bare-metal targets; wired into the Kani gate. Details:
      `docs/kani-lauf.md`.
      **Remains open:** whether in addition a Kani harness is built that carries the masking along as a
      *model* (instead of `cfg`-ing it away) and formulates reentrancy from the IRQ path as a
      property. That would be a real proof instead of a guard — but it would find
      a `cfg` selection error still not, because it would run on the same host target.
- [~] **D2 half done (2026-08-02, B-7.2).** The **copy** is gone: Loom checks the real
      `caprock-sync` source text (the script copies it unchanged, the proofs stand in the
      same file), the old replicas are deleted. The find in the process: with
      `core::cell::UnsafeCell` Loom checks only the **atomic protocol** — a weakened
      ordering in the ticket release passed ALL proofs; only with `loom::cell::UnsafeCell` do
      2 of 10 fail.

      **The IRQ masking remains open**, and it is in principle not modellable (no DAIF
      in Loom). Deadlock freedom against preemption thereby remains argument, not proof.

- [ ] **D3** The ext-29 invariants (grant non-leak, zeroing, cap budget) and the
      ext-30 invariants (directory coherence, migration lock order) have run-time tests, but
      no Verus/Kani proofs. For the migration the lock order "ascending core ID" would be a
      worthwhile Loom model (two cores migrate against each other).

- [~] **D6 (2026-08-02): the aarch64 suite had no repeat measurement — now it has one.**

      **What the finding was:** over about 13 runs **four different** checks failed
      (`xfer`, `dtb`, `prio`, `loadstop`), seemingly at random. That looked like kernel non-determinism
      and was mainly **the test mechanics**:

      1. The output hung on a **pipe**, terminated with `--signal=KILL`. A QEMU killed by SIGKILL
         does not flush its stdout buffer.
      2. After the switch to a file all runs shared **one** file. Occasionally
         **early** boot lines then went missing in the evaluated output, while all result lines were
         there — the signature stayed identical, and nevertheless a different check failed
         from run to run. Every run now gets its own file.

      **Measured after the rework:** `RUNS=16` → 16 of 16 with identical signature, `ALL PASS`.

      **What is NOT done:** in a measurement before that (shared file) 14 of 16 stood, and
      the two deviations were real watchdog runs — the line comes from the kernel, not from
      the mechanics. 16 clean runs do not exclude a rate of 12.5 % (P ≈ 12 %). The
      hang is thereby **open**, but measurable for the first time: the suite now keeps the
      full log on deviation (`build/diag/arm-abweichung-lauf-N.log`), and `RUNS` is the way to
      determine the rate.

      **Next step:** the aarch64 emergency brake should say **what** it waited for — on
      x86 it has done so since today (`bringup : offen waren: …`), and there the finding was thereby visible in
      one line. In the ARM hang all result lines are green and the report comes
      from the emergency brake nevertheless; without this line it cannot be said which condition was missing.

- [ ] **D4** Verus according to `docs/verification.md` open: `delete_leaf` on the unified structure,
      child list reachability, afterwards scheduler/IPC.

- [ ] **E-rest 3c: `hiiso` has never yet reached a verdict.** (2026-08-04, measured directly after
      the E-rest 3 fix.) The line reports in **all four** RAM sizes `SKIP` — also at 3G
      and 6G, where the shared high device table exists (`=1`). In the **load suite** it does not occur
      at all.

      | Half of the check | State |
      |---|---|
      | impermissible entries in the shared tables | is evaluated (`=0`), and a `US` mutation makes it fail — **able to speak** |
      | **private copy for an isolated PD** | `private Kopien = 0` everywhere — **runs nowhere** |

      Thereby the property against which CLAUDE.md expressly warns ("entering a device window there
      would give it to EVERY isolated PD, silently, because the cap check would run through correctly") is claimed
      for the range **above 4 GiB** and not demonstrated. The
      aggregate value is honestly `SKIP` and does not count as passed — the gap
      stands nevertheless.

      **To do:** build a case in which an **isolated PD** gets a device window above
      4 GiB, and show that (a) it has it, (b) a second isolated PD does **not**
      have it. Without (b) it is no isolation proof. The natural place is the load suite at
      6G — there are driver PDs there, and the BARs lie at `0x70_0000_0000`.

- [ ] **E-rest 1b: in the WEAK branch all four contexts share ONE window.** (2026-08-04,
      noticed when fixing E-rest 1, **not** fixed.) `slot_window` gives there every slot
      `[0, 512 GiB)`. Thereby the assurance "two contexts never allocate the same IOVA" does not hold
      — and exactly on that rests the bounds check of a driver against its **own** context.
      The weak branch is reachable from ~512 GiB RAM. Since E-rest 1 it is at least
      **reported** (`dmawin : FAILURES` instead of a silent `msi_clear=1`), but the slot separation
      is still missing there.

- [~] **E-rest 3d HALF DONE on 2026-08-04: the sites are enumerated, memory
      above 4 GiB carries, measured.** The classification stands as an enumeration at **one** place
      (`enum Zone` in `kernel/src/system.rs`), not scattered:

      | Class | who | State |
      |---|---|---|
      | `KernelOnly` — prefers **above** | thread/cap/IPC tables, kernel thread stacks, segment **and** stack frames of loaded programs, all L3 page tables, AP and secondary stacks | measured: at 3G/6G **all 28** lie above 4 GiB, both suites green |
      | `PdMappable` — must **low** | everything identically mapped (`vspace_map`/`map_frame`/`map_into_thread`) | structural: `vspace_map_page_at` rejects `va >= GIB1_END`; **counter-proof run** |
      | hard condition (`gib0_zone`) | isolated PD region, `spawn_isolated_native`, `alloc_dma_region` | get `None` instead of an unusable address |

      **The counter-proof is the actual evidence:** if one sets `system::alloc` to `KernelOnly`,
      the **load suite** fails at `-m 3G` (`drv`/`blkdev`/`dmaiso` — the driver PD never becomes
      ready), while the **main suite stays green**. A classification that checks only the main suite
      would have let the error through.

      **Two own conjectures refuted in the process, both measured instead of believed.** (a) "Loaded
      program segments need GiB 0" — false: `vspace_map_page_at` takes VA and PA separately,
      they now lie above. (b) "The device reaches only GiB 0" — false: with the virtio region
      above 4 GiB `virtio-blk` reads the sector correctly (`Geraet-DMA=1`). The first conjecture
      I had noted as a finding on the same day; it stood on a measurement that was falsified by
      another error (self-deadlock).

      The fallback counter is now **two-part** and says something: at 512M/2560M 28
      kernel allocations fall back downward (there is nothing above) — the path is thus **run**,
      not merely present; at 3G/4G/6G it is 0 in both directions.

      **What remains open is (a) the remainder of the enumeration:** EL0 kernel stacks (`claim_user_kstack`),
      the EL0 user stack of `spawn_user`, the IOMMU tables (`alloc_zeroed`) and the
      sentinel page of the DMA test continue to stand conservatively on `PdMappable`. For none of them is it
      shown that they **may** lie above — "conservative" here means unchecked, not safe.
      The kernel stacks are the most worthwhile item: 16 KiB per EL0 thread, with thousands of
      threads the determining quantity in the scarce zone.

- [ ] **D12: aarch64 fails occasionally under host overbooking — and my exoneration of the
      own rework was NOT covered.** (2026-08-04, measured.)

      I had written that the failure "also occurred before these changes". That was
      **claimed, not measured**. Made up as a bisect over the **load** instead of over the code:
      the same set-up (three parallel QEMU suites: x86 3G, x86 6G, load suite 6G), once on the
      state **before** the VA window (`986bb40`, own worktree), once afterwards.

      | State | Runs under threefold parallel load | green |
      |---|---|---|
      | before the rework (`986bb40`) | 6 | **6** |
      | after the rework (HEAD) | 6 + 12 | **17** |

      **This measurement could NEVER answer the question — and that should have been noticed
      before the start.** At the observed rate of 1/18 ≈ 5.5 % a 6-run before yields with about
      **71 %** probability zero failures, *even if the rate is identical*
      (`0.945⁶ ≈ 0.71`). Fisher over 0/6 against 1/18 gives **p ≈ 1**. I thus traded wall time against
      a result that was equally probable under both hypotheses — a
      measurement that feels like insight and is none. For a distinction it would need
      roughly **50+ before runs**.

      **The lesson belongs to "falsify cheaply":** before the start check whether the set-up
      can separate at the *expected effect size* at all. A bisect over the load is
      cheap in engineering and expensive in wall time — exactly there the calculation beforehand pays off.

      **What remains.** My exoneration ("also occurred before") was unproved and still is;
      the measurement neither supported nor refuted it. The difference from before is
      only that I now **know** that.

      **What is missing, and that is the actual deficiency:** I have **no log** of a
      failure. A `tools/` replica stands (`armfang.sh` in the scratchpad keeps the full log on
      deviation), but has not triggered in 12 runs. Without the failing line every
      conjecture about the cause is prose.

      **The three failures probably belong together — and the prior lies with the SCAFFOLD.**
      aarch64 under parallel load, x86 load suite at 512M sequentially, x86 `RUNS=8` with a
      log whose **signature is identical to that of a green run**. The last finding
      is the most telling: if all result lines are right and the run nevertheless
      fails, a check breaks that has **nothing to do with the property checked** —
      truncated output, time limit in the expectation comparison, a lost last character.

      In this session the measurement set-up was convicted **three times** (`tail -1`; three
      suites without a log; a retention block behind the deletion). After three hits the
      obvious hypothesis is no longer "three rare kernel errors", but **one scaffold error**.
      To treat the three together and search there first is cheaper than three separate
      hunts.

      **The prior needs an EXIT, otherwise it tips over.** "Check the scaffold first" is right after
      five hits — but without a criterion fixed in advance it becomes in three weeks
      "it was certainly the scaffold again", the same unfalsifiable convenience as "also occurred
      before", only with the opposite sign. Therefore written down **now**, as long as the
      prior has not yet become dear to us:

      **The criterion hangs on the FORM of the artefact, not on repeatability.** The first
      version demanded a *reproducible* check line — and would thereby have excluded exactly the class
      at issue: a missed `WFE` wakeup or a timer/IPI race
      at 1 in 32 does not reproduce and would have fallen for ever under "scaffold". That would have given the
      prior a door through which it cannot be refuted — the same convenience
      as "also occurred before", only with the opposite sign.

      A failure is a **kernel finding** if **all three** features apply — a
      **single** run can fulfil that:
      1. the output is **complete up to the end** (final line present),
      2. the **exit code fits the verdict** (no `exit 0` with `FAILURES`, no abort),
      3. a check line **deviates in content** — values, counters or order are different,
         not merely shorter or missing.

      Everything else — truncated output, missing final line, time limit in the
      expectation comparison, exit code without a matching verdict — is until further notice a
      **scaffold finding** and is searched for there.

      **Repeatability belongs in the prioritisation, not in the classification.** A
      one-off kernel finding is a kernel finding; that it is harder to hunt changes
      nothing about what it is.

      **Do not file as "environment".** An error that occurs only with overbooking of the host
      is a candidate for a missed `WFE` wakeup or a timer/IPI race — time-dependent
      kernel errors live exactly there, and the emulation only shifts the probability,
      not the cause. Related to D0 (x86) and D6 (aarch64), but with an own,
      **reproducible trigger**: load.

      **The same picture on x86, and the same deficiency.** The **load suite at 512M** failed on
      2026-08-04 twice — both times within a longer collection run, afterwards in isolation
      6 of 6 green (and before that already 3 of 3). For that too I have **no log**.

      **The logs were not "thrown away by my loop" — they never existed.** I had
      written that the suites themselves file a log on deviation and only my
      collection loop had discarded it. Checked (2026-08-05): that holds for **none** of the
      three. The load suite used a `mktemp` and deleted it at the end **unconditionally**; the
      x86 suite deleted its own directly after reading it in, long before the checks; the ARM suite
      kept something only on a deviating **signature** of a repeat run. In `build/diag/`
      there is nothing of 2026-08-04.

      **Fixed, and the retention has been run.** All three suites now file the full log
      under `build/diag/` on `fail != 0`. The first attempt was itself a silent checker:
      the block stood at the end of the file, but the log had been deleted by that time — it **could**
      never fire. Counter-proof run with a forced failure: 176
      lines (x86 suite) resp. 193 lines (load suite) filed.

      **The retention fired at once — and in doing so showed the next hole.** On
      2026-08-05 `RUNS=8` failed on x86; the kernel log was present this time
      (`build/diag/ABWEICHUNG-…`, 176 lines). Result of the evaluation:

      * the **signature** of the filed run is **identical** to that of a green run
        (diff empty) — the failure was thus **no** signature deviation,
      * no `abweichung-lauf-N.log` arose either, the repeat comparison thus
        likewise did not respond,
      * so the check that failed must be one of the **derived** ones (time limit,
        `checks` section) — and **its line stands in the stdout of the suite**, which my
        collection loop again kept only as `tail -1`.

      **The same gap, one level higher.** The suite now keeps the *kernel* log; what
      was missing was the *verdict output*. Three further `RUNS=8` passes (24 runs) afterwards were
      green.

      **No point estimate from that.** "The rate is 1/32" would be exactly the error
      from above, one level higher: one failure in 32 runs gives a 95 % interval of roughly
      **0.5 % to 16 %** — a number that is noted as a baseline makes every later measurement
      unfalsifiable. What can be fixed: **one failure in 32, interval wide, rate
      undetermined.** And without the check line it is not even known **which** statement breaks.

      **(1) done on 2026-08-05:** `tools/sammellauf.sh` keeps the **complete stdout** per
      run. Two own errors of this file were noticed in the process, and the second turned the
      balance question around:

      * Success was defined as "`ALL PASS` in the final line" — and thereby filed the
        logs of the **green guards**, which close differently.
      * An **empty** final line counted as success. That did not *lose* a failure picture, it
        **invented a success**: a run that ended in the middle of the output was booked
        green. **Thereby the green balance is damaged, not only the red one** — and because on
        success it was deleted, it could not be recounted how often.

      **Reach, and this time checked for ALL measurement series of this session — not only for
      the new collector.**

      | Build | Green criterion | fail-safe? |
      |---|---|---|
      | `sammellauf.sh` (1 run, before the fix) | empty line counted as success | **no** — `load 6G` affected, repeat then really green |
      | `armlast.sh` (aarch64 bisect, 6+6 runs) | `[ "$r" = "== ALL PASS ==" ]`, exact character comparison | yes |
      | `armfang.sh` (12 runs) | `grep -q "ALL PASS"` on the final line | yes |
      | the `for M in …` loops (RAM series, `RUNS=8`) | last line **printed**, read by me | yes, with restriction |

      The reason why the last three carry stands in the source text of the suites and was
      checked: `== ALL PASS ==` is written **exclusively** in the `fail = 0` branch as the last
      output before `exit "$fail"`. An aborted, truncated or failed run
      cannot carry this line as the last, and text and exit code cannot diverge
      in the green direction. The restriction with the loops is human (I read the lines), not
      mechanical — and empty lines I noticed both times.

      **Thereby the green balance of this session stands**, with the one named exception.

      **Two stipulations from it.** (A) The outcome is decided at the **exit code**, not at the
      text — otherwise a formula grows with every new suite and with it a hole. The text is
      only cross-read; a contradiction is a finding **about the suite**. (B) Logs are
      **for now also kept on success**, until the file has carried a few dozen runs.
      Five cases run as counter-proof (success, guard formula, empty line, `exit 0` with
      FAILURES, `exit 1`).

      **To do:** (2) lead all collection runs via `sammellauf.sh` and let them run until
      a check line is present; (3) **check the scaffold first** (see above), not the kernel; (4) measure both
      states with **50+ runs each** — anything below cannot separate at this effect size.

- [ ] **E-rest 3f: the teardown RE-DERIVES the address, instead of consuming what the mapping
      returned.** (2026-08-05.) `unmap_dma_from_thread(tid, phys, len)` takes the same raw
      values once more and computes the VA again. That it hits the same axis in the process is today held by a
      **constructor name** (`Va::for_dma_window` at both places) and the
      guard that counts it — i.e. findability, not impossibility.

      **The structural version:** the mapping returns a **handle**, the teardown consumes
      exactly this handle. Then the address in the teardown is never again a free value, and the
      asymmetry is **unconstructible** instead of findable. That fits the existing
      teardown tokens (ext-37) and the `Owned<T>` typestate of the virtio crate — the same construction,
      one level deeper.

      **The handle must be bound to the PD**, otherwise the next folding arises: a handle
      from PD A that tears down in PD B would again be the same value with two meanings. The binding
      belongs in the type (ASID or PD id in the handle, checked by the teardown), not in a
      calling convention.

      Order: after E-rest 3e (a), because the window rework touches the signature anyway.

- [ ] **E-rest 3e: DMA regions hang on GiB 0 — and those are TWO questions, not one.**
      (2026-08-04, newly decomposed during the VA==PA pass; the first version of this entry folded
      both together and thereby made the item larger than it is.)

      **(a) Mapping — the same work as E-rest 3d.** The region is mapped via
      `map_region_into_thread`/`MappingKind::Dma` **identically** into the driver PD
      (`IdentityReason::DeviceDmaWindow`). That is the CPU side, and for it applies literally what
      applied to the private PD region: the window solves it. No new design needed.

      **(b) Allocation — a completely different question, and the actual gap.** A 32-bit-capable
      device can write only into the lower 4 GiB. That is **no property of the mapping**,
      but a restriction on the **allocation** — and under VT-d/SMMU it even dissolves:
      the **IOVA** can be low while the PA lies anywhere. GiB-0 pinning is thereby a
      **policy of the allocator**, which belongs chosen at the IOMMU detect, and no property of the
      region.

      **What is really missing is expressibility — and it belongs on the DEVICE side.** A
      `dma_mask` in the **memory** cap would be the same folding one axis further: the 32-bit boundary
      is a property of the **device**, not of the memory region. If it stood in the
      memory cap, memory would arise that is "for 32-bit devices" and fit for nothing else —
      an offer that carries the limitation of its consumer. (So it stood until 2026-08-05
      in this entry; false.)

      Right: the restriction belongs on the **device cap** resp. on the DMA domain, and the
      allocation is then a **join** of region offer and device restriction — the same form
      as "colour AND zone in one decision" (E-rest 3b) and as colour+NUMA (Z8).

      **The failure case of the join — empty intersection — belongs at the cap DERIVATION, not at the
      DMA time.** A device cap for which no permissible offer exists should not even
      be producible. Otherwise a cap arises that looks like authority and fails at
      first use — the same form as a manifest entry whose selector matches no
      device (A-5.3), and there the answer is already "reject instead of guess".

      Today the suite reports the statement as absence
      (`dmawin : Geraete-ohne-deklarierte-Adressbreite=1`) — honest, but a **counter for a
      missing statement** and no statement. As long as it is missing, every choice of the base address is
      a guessing step: lying below means "safe for all devices", the same sort of precaution
      as "bottom first" from E-rest 3b — it costs the scarce zone and proves nothing.

      Order: **(b) before (a)**. (a) without (b) would be a leap of faith to unknown
      hardware — the region would then lie above because it works, not because the device can do it.

---

## F. Take debug/test code out of the release build

**Finding (measured):** about **8,000 of the 28,800 lines** — a good quarter — are checking infrastructure,
and it is compiled along and executed at every start:

| | Lines |
|---|---|
| `threads/mod.rs` (demo threads + test services) | 5,402 |
| `threads/fuzz.rs` | 1,299 |
| `selftest.rs` | 363 |
| `arch/x86_64/dmar_selftest.rs` | 249 |
| `dmatests.rs` | 186 |
| `testsupport` in `system.rs` | 558 |

In addition **364 telemetry statics** (`static X_OK: AtomicBool`), of which 339 in the test modules themselves,
and 234 `println!` calls over 731 lines. The fuzzer already hangs on `kernel-fuzz`; the rest
does not.

**The dependency is one-sided** — checked: neither `system.rs` nor `loader.rs` nor any
crate calls into the test modules. There are exactly **eight** call sites (`main.rs`, `bringup.rs`), and
`testsupport` is used outside the test modules at only **one** place (`peek_dma_words` in
the virtio demo).

- [ ] **F3** Do **not** gate boot messages (`mmu:`, `apic:`, `mem:`) along — they are no
      debug code, but the only sign of life of a kernel without a console. That the HAL with
      8,401 lines contains only **seven** `println!` shows that the separation is already
      observed there; the boundary runs between bring-up message and test report.

---

## G. Known architecture limits (documented, no bug)

- IPC transfers **4 register words**; everything larger via shared memory.
- TrustedSAS PDs share an address space — a wanted consequence of intralingual isolation
  (safe Rust cannot forge a pointer to foreign memory); the certificate gate enforces the
  precondition (`unsafe_status == ALL_PASS`).
- Tier 3 of verification (functional full correctness, info flow, HW model) is research class.

---

# Part II — Linux userland and local graphics on Caprock (formerly `TODO0.md`)

**Class:** strand plan · **State:** 2026-08-17, **not begun** · **Scope decided,
effort estimated and marked as an estimate.**

## 0. The relationship to Part I — please read first

Part I (formerly `todo.md`) is the source for "what is open". **This part copies nothing from there.** Where an
item already stands in the register, there is a **pointer** here and the state in one sentence. Own
`[ ]` entries (`K*`, `P*`, `G*`, `L*`) exist only for what is **not yet** in the register.

The reason is the trap this project has already paid for: *two numbers that come from the same
hand are not two sources.* Two lists with the same items drift, and the day on
which they contradict each other makes the question "what is open" unanswerable.

**Rule for this part:** if a `K*`/`P*`/`G*`/`L*` item is begun, it moves **to
Part I** and the pointer remains here. This part is a **plan**, not a second register.

---

## 0b. What this strand belongs to

**The goal has stood in `CLAUDE.md` since 2026-08-17:** Caprock OS in the end entirely in Gabbro —
kernel, drivers, desktop —, formally verified; alongside it unverified things for usability
(Linux drivers, binary compatibility). The promise is **"the unproved cannot
break out"**, not "everything is proved".

**For this strand exactly one ordering rule follows from it, and it is binding:**

> **First the confinement, then the proof.**
> Every item below is assessed by whether it carries the **boundary** — not by whether it is later
> provable. A Linux driver in an IOMMU-bounded PD fulfils the goal today; a
> proof about it never will.

What that concretely decides: **D3** (a crashing applet takes nothing along) counts more than
any line of compositor code, and **A6/`CAP_IRQ`** counts more than any optimisation — because both are
boundaries. And: the later Gabbro version concerns **the compositor core**, not the
toolbox (see §7b).

---

## 1. The scope — what this strand is and what it is not

**Goal:** a system that sits on top of Caprock, is locally usable graphically (Hyprland as
the target application) and keeps the kernel's isolation promise. Expressly **without a CLI claim** —
that is a relief, not a gap, see §1.2.

### 1.1 Two projects that the question mixes

| | "Linux **drivers** translated onto Caprock" | "**Hyprland** runs" |
|---|---|---|
| missing | a **layer** that executes Linux driver code without Linux | the **userland substrate** |
| Stage here | **L** (§8) | **K**, **P**, **G** (§5–§7) |
| Precedent | Genode `dde_linux` (proven, **GPLv2** → friction with AGPL) | Starnix (BSD-3) — not **needed** here, however, see §2 |

**They are independent.** G (graphics via `virtio-gpu`) needs **no** L. L needs **no** G.
Whoever throws both into one strand makes the cheap half dependent on the expensive one.

### 1.2 What dispensing with a CLI saves

No `fork`/`exec` process model, no job control, no TTYs, no users/groups, no
`/proc` process tree, no shell semantics. The dispensation is thereby the biggest single
cost reducer of this strand — it strikes exactly the surface on which WSL1 failed
(behavioural fidelity of file system and process model).

> **Re-measured 2026-08-17 — and "a compositor needs none of this" was true only by coincidence.**
> Hyprland imports `fork`/`execl`/`execv` (for the `exec-once` directive and **Xwayland**) and
> `dlopen`/`dlsym` (for **plugins** and the crash report). Both are **no core path** and
> can be switched off — the dispensation thus holds, but **only after a decision**, not by itself.
> `dlopen` also comes back as soon as **Mesa** is added: it loads its drivers that way.
> Sites and symbol count in `~/Dokumente/Caprock OS/MESSUNGEN.md` M1b.

### 1.3 What this plan does NOT promise

* **No** "arbitrary Linux binaries run". The way is **recompiling** (Z16), not
  binary ABI fidelity.
* **No** CUDA. That is Z26/way A and stays outside here (see §2).
* **No** bare metal. The target is QEMU, until stage G stands.
* **No** "only Caprock as TCB", as soon as a compositor runs — see §9. The sentence is **not**
  negotiable and belongs in every external communication of this strand.

---

## 2. The decision that makes this strand cheap — it has already been made

`TODO.md`/Z26 names the case literally: *"Only display/compositing → **way B**"*. Way B is
`nouveau` + `NVK` resp. `i915` (**728 of 900 files WITH**, measured) — all source text, all
translatable.

**Consequence, and it is the biggest cost block of the whole question:** the **binary Linux ABI**
(Z26/A3 — `ld.so` for foreign ELFs, glibc symbol surface) stays **off the critical path**. It is
needed only for the proprietary CUDA blobs, and those are not on the list in this scope.

> **The recorded decision "CUDA YES → way A" (2026-08-09) is not reversed
> by this.** It applies to the GPU compute strand. This strand here is the other branch
> of the same fork and runs alongside. Whoever wants both pays both — but not in this
> order, and not as one project.

---

## 3. The dependency order — it is different from what the question suggests

**The hardest blocker is not graphics** — but it is also not the one that stood here first.

> **CORRECTED 2026-08-17.** Here stood: *"A PD has ONE thread today (0 hits for
> `SPAWN`/`CLONE` in the ABI)"*. That was taken from `todo.md`/Z14 and is there **outdated since
> 2026-08-10**: the kernel half of Z22 P2 is built and is measured in **every** suite run
> as `pdthrd` (two threads in PD 1, shared cap slot, open transaction).
>
> **The blocker is one level narrower: what is missing is the ABI way** (`SPAWN`) by which a
> userland program creates a thread for itself — see K1a. Mesa, Aquamarine, smithay and every
> JVM need `pthread_create`; the kernel mechanics beneath it already stand.

That `futex` carries ⬛ 67.8 % of the run time in Hyprland and ⬛ 96.9 % in the JVM remains unaffected
by this — it is still the quantity on which the personality stands or falls.

From it the order. Every stage is useful on its own, even if the next never comes — that is
the acceptance condition of the scope, not a courtesy:

```
K (Substrat)  →  P (Persönlichkeit)  →  G (Grafik)
                                         L (Treiberschicht)  ← unabhängig, nur für Blech
```

| Stage | Use, even if the strand ends here |
|---|---|
| **K** | threads per PD, `mmap`, fault redirection — every item stands on the way to the server OS (Z20) anyway and is acceptable on its own |
| **P** | a `libcaprock` musl carries *every* program built from source, not only compositors |
| **G** | `virtio-gpu`/`virtio-input` are two further driver PDs — the same form as `virtio-blk`/`-net`, hence proofs for A-5 |
| **L** | unlocks **all** Linux drivers (WiFi, audio, USB), not one |

---

## 4. Stage 0 — what already applies from `TODO.md` (pointers, no copies)

| Item | Register | State in one sentence |
|---|---|---|
| Syscall redirection primitive | Z26/A3 | **built**, `redirect : ALL PASS` — but **1.5 of 4 authorities**: guest memory access and vspace manipulation are missing |
| Syscall ABIs as modules | Z28 | decided, not begun; cut along the **shared state**, not the syscall number |
| `CAP_IRQ` / IRTE | B-3, Z20 | encoding **and** allocation stand as a pure function (10 host tests), **nothing is wired** |
| Process start stack, TLS, stack size, SSE | Z19/A1–A4 | A4 done, A1–A3 open — **without them no `main(){return 0;}` starts** |
| Memory server in userspace | Z14 stage 1 | open, but possible **without a single new kernel line** |
| File system beyond FAT16 | Z20 | open; today FAT16 over **one** file |
| Desktop classification | Z20 | "two to three orders of magnitude", **hobby milestone**, does not serve the product thesis |

**These lines are not maintained here.** If something different stands there, that applies.

---

## 5. Stage K — the substrate (kernel; the only thing that costs TCB)

The four kernel operations from Z14 are the foundation here; **three of them are open.** Rough
size there: 500–800 lines, ~5–8 % TCB growth. Each is useful on its own and can be accepted
individually.

- [~] **K1. Several threads per PD — THE KERNEL HALF IS BUILT. Corrected 2026-08-17.**

      > **This entry was wrong, and the error is instructive enough to stay standing.**
      > It said "today a PD has one thread (0 hits for `SPAWN`/`CLONE` in the ABI)" and
      > made K1 the **hardest blocker of the whole strand**. I had taken that over from
      > `TODO.md`/Z14, where it stands as a table row — **and there it has been outdated since 2026-08-10.**
      > The measurement stood the whole time in every suite run, also in my own
      > of today; I never grepped for it. *A register that keeps finished items makes the
      > question "what is open" unanswerable* — and then somebody builds a plan on it.

      **Built (Z22 P2), measured in every run as `pdthrd`:**

      ```
      pdthrd  : EINE PD, ZWEI Threads: pd(server)=Some(1) pd(client)=Some(1) (PD 1)
                gebundene-Threads=2 (erwartet 2) · erstes-RECV-Code=0
                (beide Threads sehen DENSELBEN Cap-Slot; nur einer gebunden hiesse 4 = ERR_NOPD)
      ```

      In addition in the PD table: `nthreads` per PD, a **reverse index** (tid → pd) and
      `any_thread(pd, predicate)` as a replacement for "take `thread_of(pd)` and check that one" — O(threads)
      instead of O(PDs × threads), with a named fallback to the representative as long as the table has no
      memory. And the Z23/S1 gates are measured **at the open transaction**, for which two threads
      in the same PD are needed in the first place.

      **What REALLY is missing — and it is much smaller:**

- [x] **K1a. An ABI way by which a PD creates a thread for itself — BUILT AND MEASURED
      (2026-08-26). This entry was last wrong in two directions.**

      > **Corrected 2026-08-26.** Here stood "the ABI has 19 syscalls, no `SPAWN`". The
      > syscall `sys::SPAWN = 20` has stood in the ABI since **2026-08-17**, with
      > `spawncheck::check_stack`, six named rejections and `ERR_INUSE`. The line had been outdated
      > for nine days.
      >
      > **And the other direction was worse:** `SYS_SPAWN` had, until 2026-08-26,
      > **no caller and no gate** — 0 hits in the whole tree outside its
      > definition and a number anchor. It was built and **never executed**. A register
      > that says "missing" where "unmeasured" would be right sends the next plan to the
      > wrong place — the same form as with K1 itself.

      **Measured as `arena` in EVERY run of both x86 suites** (arch-neutral, aarch64 runs
      it along):

      ```
      arena  : ALL PASS (fertig=true ganze-region=true vier-fenster=true disjunkt=true
                lebendig=true threads=6 slots=2 ueberlappung-abgewiesen=true
                ausserhalb-abgewiesen=true cap-gesperrt=true | maske=0xf magisch=4/4
                korrupt=0 codes: ganz=0 ueber=7 aus=21 del=18)
      ```

      The acceptance condition from this entry is redeemed, only with a different quantity than
      the relay baton: every child deposits a word derived from **its** window base and
      keeps reading it back — "`spawn` gave `Some`" does not count, and four threads on one arena
      would have looked the same without this line if they trample each other. The overflow case
      (`ERR_THREAD_LIMIT`, non-blocking) stands in `spawncheck` with a host test.

- [x] **K1b. Several stacks from ONE cap — the sub-region (2026-08-26).**
      Until then a `Memory` cap bought exactly **one** thread, and it remained locked for its life.
      "How many threads may a PD have" was thereby "how many cap slots are still
      free" — the wrong question: threads of a PD share the address space anyway, a separate
      cap per stack buys **no isolation**. A driver with 6 of 8 occupied slots got to two
      threads.

      `x1` of `SYS_SPAWN` now carries `(offset_pages << 32) | length_pages`; `0` is **the whole
      region** and thereby bit-identical to every call written before. Four rejections with their own
      names, among them `ERR_SUBREGION` (a window outside the cap is something different from a
      region that is not fit as a stack).

      **The finding on the way, and it is the more important one:** the `Overlaps` rejection was for exactly
      the threads that `SYS_SPAWN` creates **structurally unreachable**. `pd_mapping_overlaps`
      reads `KSTACKS.ubase_of`, and `spawn_with_stack_parked` **deliberately** enters nothing
      there (the region belongs to the cap, not to the kernel). The checker could not see the case against which
      it is built — and with several stacks in a cap that is exactly the
      main case. Since then there is `stack_sibling_overlaps` beside it. Counter-proofs:
      `tools/spawnarena-negativ.sh` (M1 is literally yesterday's tree).

      **Remaining open:** the window between `admit` and the entry in `STACK_CAP_OF` — two
      threads of the same PD on different cores can both check before the respective other's entry.
      The same window that the `ERR_INUSE` protection has had since K1a; within one PD
      the same trust zone, a hole as soon as `SPAWN` ever crosses a PD boundary.

- [ ] **KB. Stage B (`CAP_IRQ`) — planned 2026-08-26, `docs/plan-cap-irq.md`.**
      ⬛ The entry "IRQ delivery is missing" was wrong in **both** directions, the same form as
      K1a: the delivery mechanics are built and are measured on aarch64 in every run as `irq : ALL PASS`
      (RTC via GIC SPI); the IRTE encoding including SVT/SID and hardware access also stands.
      What is missing is narrower: **MSI-X programming** (`pcie.rs` knows no MSI),
      **a caller** for `vtd::irte_vergib`, and a **cap latch** at `bind_irq` — whose
      doc comment "authorised via the IRQ cap" claims what its signature cannot do.

      The load-bearing design decision stands in the plan: **the driver writes its MSI-X table
      itself** (it holds the BAR cap anyway), and the IRTE indirection is the latch — a handle
      that it has not been given hits a non-present entry or a foreign SID, and
      VT-d rejects it. Without `SVT_SID` the interrupt delivery would be exactly the channel that A-5.4 closed
      on the DMA axis.

      **Decided 2026-08-26 (E10-E12), all three smaller than they looked:** the re-trigger
      protection is an **assurance** instead of a shared implementation (aarch64 masks,
      MSI is edge — it is checked identically on both: trigger twice before the drain, at most one
      delivery); the **kernel** writes the MSI-X row and rejects a device whose table lies in
      the offered BAR (measured: the driver today gets exactly the BAR with the common config, anything else
      would be a coincidence of the selection rule); and the binding hangs on the
      **allocation** instead of `NIRQ_BIND` — one vector per device is already the upper bound, an account
      is not needed.

      **And the order A→B holds only half:** the positive direction ("the driver made progress without
      a single poll round") is measurable today; the counter-proof needs a deadline, but
      **that of the measurer, not that of the ABI**. Stage B is thus acceptable before stage A. What A
      really needs is the product: a driver must be able to **recover** from a missing interrupt,
      and that does not work without `ERR_TIMEOUT`. **Named as debt:** *before stage A a driver PD
      whose interrupt fails to appear hangs irrevocably* — a property of the delivered state, not a latent
      defect. A poll fallback as an interim solution is **forbidden**: it would undermine precisely
      `poll-runden == 0`, i.e. the load-bearing conjunct of `irqmsi`. No interim solution between B and A.

- [ ] **K1d. The PD-local preemption gate — the only requirement that follows from E3.**
      Decided 2026-08-26, reasoning in `docs/linux-kompatibilitaet-caprock.md` §5.

      Caprock is **preemptive, but without parallelism within a PD** (`SYS_SPAWN` puts every
      thread on the caller's core — measured, not assumed). That is
      `CONFIG_SMP=n` + `CONFIG_PREEMPT=y`, a supported Linux configuration, and its answer
      stands in `include/linux/spinlock_up.h`: `spin_lock() → preempt_disable()`, no spinning.
      **Thereby `spin_lock` and `rcu_read_lock` stay in class A** — the A layer needs no
      real locks.

      **The cut is the decision, not the whether:**
      * **no global scheduling override** — that would be a system DoS from a driver PD, i.e.
        exactly the authority that a PD must not have;
      * it means *not switching to another thread **of the same PD***. Preemption by
        a foreign PD is harmless, because no state is shared;
      * **capped by the remaining SC budget**, otherwise "held" is unbounded and a new
        failure picture arises instead of a named one (D11 form).

      **Acceptance:** two threads of the same PD, one holds the gate and writes a
      two-word state that is inconsistent in between; the other reads it in a loop.
      Without the gate the reader must **see** the inconsistency (positive control — a test that
      never sees it measures nothing), with the gate never. In addition: a thread of a **foreign** PD
      continues to preempt (otherwise it is a global override after all), and the gate falls
      by itself at the end of the budget.

      **Precondition that stands and falls with the decision:** as soon as `SYS_SPAWN` gets a core choice,
      `CONFIG_SMP=y` applies and `spin_lock` leaves class A. The core choice does not exist
      today — whoever introduces it must open this entry along.

- [ ] **K1c. `NCAPS = 16` is the REAL cap per PD — not the budget.**
      Found on 2026-08-26, and by the self-test, not in proofreading: `CAP_BUDGET_MAX`
      stood at 64, but the **local cspace of a PD is an array of `NCAPS` = 16 slots**
      (`Pd::cspace`). `install_cap` rejects every slot `>= NCAPS`, **without ever asking
      the budget** — a budget of 20 was not a generous promise, but an unfulfillable one.
      `CAP_BUDGET_MAX` has since been `NCAPS`, with `const _: () = assert!` beside it.

      Thus the number stands that really limits a driver environment: **16 slots per PD, hard.**
      The budget account (see A3) raises the reachable number from 8 to 16 without costing the other
      ten thousand PDs anything; beyond that a **variable** cspace per
      PD is needed. That is a change of its own and touches `Pd`, `PdTable` and every site that passes
      `[Option<CapPtr>; NCAPS]` around as a value (`caps_of`).

      **Four decisions stand before it, and none is arbitrary:**
      1. **Where does the stack come from?** From a cap of the caller (then it bears the cost and the
         policy stays outside) or from the kernel (convenient, but a second memory policy).
      2. **The upper bound per PD — and its NAME.** *Whoever introduces a capacity must name the overflow*
         (D11 literally). Without its own error code a bound is no protection but
         a hole — and a program that creates unbounded threads is a DoS channel like
         `AUFTRAEGE_MAX` at C8.
      3. **Admission order.** `spawn_parked` → set stack/TLS → `bind_pd` → `admit`. A
         thread that is runnable before it has its authority is **D0 literally**.
      4. **TLS base per thread** (Z19/A2) — without it `errno` in musl is broken, i.e. the libc
         as such, not "threading".

      **Acceptance** (still valid): a relay baton that **both** threads must advance in turn,
      plus their **own** preemption counts — *"`spawn` gave `Some`"
      does not count.* Counter-proof: one thread frozen → the relay baton stands. In addition an
      overflow case that gets the named error code **and does not block** (D11).
      **TCB:** yes — one syscall.

- [ ] **K2. Map/unmap into a FOREIGN PD.**
      `SYS_MAP` today maps only into the **own** VSpace. Without it there is no `mmap`, no
      `mprotect`, no loader for reloaded libraries — and no memory server that
      *fades in* something for its client instead of handing over a cap.

      **Acceptance.** PD A maps a region into PD B; B reads what A wrote; **PD C
      does not see it** (positive control over the same path, only one cap moves — the same
      form as A-5.4). **TCB:** yes, cap-gated like `PDCTL`.

- [ ] **K3. Fault redirection to a PD.**
      Today **a fault terminates the thread in the kernel**. Needed for COW, demand paging
      and `MAP_SHARED`. Z14 rightly calls it "the first step towards a pager in userspace".

      **Acceptance.** A guest faults on an unmapped page, a handler in another
      PD gets address and access type, maps, the guest **continues running and sees
      its value** — the same form of acceptance with which the payload of the redirection primitive
      was proved. Speak check: without a handler cap the thread must die as before, and the line must be able to
      **distinguish** that. **TCB:** yes.

- [ ] **K4. Guest memory access for handlers (addendum 2 to Z26/A3, only pointed to here).**
      Without it **every syscall with a pointer** is not implementable — `read`, `write`, `ioctl`.
      Stands in the register; noted here as a precondition of P, so that the chain is complete.

- [ ] **K5. Time as an interface.**
      A compositor is time-dependent through and through (frame clock, input time stamps, animation).
      Cycle accounting exists (`caprock-sched/src/cycles.rs`); a **clock** for userland does not.
      Z19/B names `clock_gettime` as the vDSO page — the cheapest way, because it does not
      touch the kernel.

      **Acceptance.** Monotonicity over 10⁶ queries, and the **resolution** as a number in the report (not
      "works"). Lower bound beforehand, otherwise zero is green. **TCB:** one shared, read-only
      page.

---

## 6. Stage P — the personality (userland; TCB-neutral)

**Principle:** no kernel growth. Everything here is a PD.

- [ ] **P1. `libcaprock` musl.** Z16 line: the same source text, **relinked**. Presupposes Z19/A1–A3
      (`argc`/`auxv`, TLS, a stack that is not 16 KiB — usual are **8 MiB**, and
      that belongs in the manifest, not in a constant).

      **Acceptance.** An unchanged foreign program from source (proposal: `zlib` self-test,
      because it has few dependencies and a built-in verdict) builds and passes.

      **The scope measurement has been run — but in TWO versions, and only the second applies here.**
      `strace -c -f` gave **84 distinct syscalls** and answers the breadth gate
      ("personality or something smaller?", threshold 150). For the **recompilation path** it
      is the wrong quantity: relinked, Hyprland issues no Linux syscall, it calls libc.
      What is decisive is the **symbol surface**: ⬛ **1,069 undefined symbols, of which 139 from libc
      and 140 from libstdc++**. These 139 are the scope of P2, not the 84.

- [ ] **P2. The five load-bearing syscalls of a Wayland event loop.**
      `futex`, `epoll`/`poll`, `eventfd`, `timerfd`, `memfd` (Wayland shm uses `memfd` with
      sealing). After Z28 they fall into the groups *synchronisation* (small) and *fd/files*
      (**large**) — the cut follows the shared state.

      **Acceptance.** For each syscall a case that **needs** it and demonstrably hangs without it
      (counter-proof with exactly one open conjunct, house form).

- [ ] **P3. Unix sockets with fd passing — and that is structurally cheap.**
      **Wayland *is* `SCM_RIGHTS`**: every buffer, every dmabuf travels as an fd over the socket.
      Handing over an fd is **handing over a cap**, and cap transfer in IPC has stood for
      a long time. That is the one point at which Caprock's model *meets* the goal instead of standing
      in the way.

      **Acceptance.** Two PDs, one socket, one buffer travels; the receiving PD writes into it,
      the sending one reads it. **And a third PD that does not have the same socket sees nothing.**

- [ ] **P4. VFS with paths and device nodes.**
      Needed: `/dev/dri/*`, `/dev/input/*`, `/sys/class/drm` (udev enumeration), plus a place
      for Mesa's shader cache. Today: FAT16 over **one** file. Z20 lists this as "large, but
      dependency-free like `caprock-part`/`-fat`" — the design thus stands, the scope does not.

      **Acceptance.** A program opens a path that it does not know, via an enumeration —
      not via a compiled-in constant. Otherwise the test checks the constant.

- [ ] **P5. Memory server (Z14 stage 1, only pointed to here).**
      Possible without a new kernel line, acceptance stands there. Precondition for `malloc`.

---

## 7. Stage G — graphics, and the finding that is missing from the register

### G0. The shortcut finding: **DRM need not be emulated**

Hyprland's hardware access does not go directly to DRM, but through **Aquamarine** (formerly
`wlroots`) — a **backend abstraction** with exchangeable back ends (DRM, nested
Wayland, headless). A **native Caprock backend** replaces the entire `/dev/dri` `ioctl` surface
with a cap interface with about a dozen operations.

**Why that is big:** `ioctl` is not a syscall, but an **unbounded API per driver**.
Whoever emulates DRM emulates modesetting, atomic commit, GEM, dma-buf, `drm_syncobj` and
vblank events — a surface without edge. Whoever writes a backend implements **one**
named interface. The same movement as A-5.1: the policy moves into a PD, instead of the
kernel rebuilding a foreign surface.

> **Thereby way A falls away twice** — not only for CUDA, but also for the graphics `ioctl`s.

**DECIDED on 2026-08-17, measured** (log: `~/Dokumente/Caprock OS/MESSUNGEN.md` M2).
`IBackendImplementation` has **16 purely virtual methods in ONE header**. The existing
implementations directly yield the cost scale: **null 89 · headless 264 · Wayland 887 ·
DRM ~4,526** lines. **Counter-proof on an independent tree:** wlroots' `backend/headless/` is
**246** lines — practically the same number with different code, different language, different project.

> **Two independent sources say: a minimal backend costs about 250 lines.** What is saved
> is the DRM surface of ~4,500 lines — and, more important, its character.

**G0 is thereby no longer a hypothesis.** The abort condition in §11 about it is done.

- [ ] **G1. `virtio-gpu` and `virtio-input` in `crates/caprock-virtio`.**
      The same `Transport`+`Queue` scaffold that `blk` and `net` already carry; the same QEMU scaffold
      in which both suites run. **The cheapest line of this whole plan**, because it invents
      nothing new.

      **Acceptance, and it must satisfy the house rule** — *a writer that confirms its own result
      confirms nothing.* The frame is **not** read back by the driver, but
      fetched from QEMU via QMP `screendump` and checked **outside** against an expected pattern
      (different tool, pattern written out again there instead of imported — like
      `tools/checkfat.py`).

- [ ] **G2. Soft compositing first.** Pixman instead of Mesa. Slow, but it separates "the picture
      stands" from "the GPU computes" — two statements that otherwise fail together and then
      cannot be told apart.

- [ ] **G3. The compositor — and the choice was TURNED on 2026-08-17.**
      Not Hyprland first, but **smithay** (Rust). Measured: **all** C bindings are
      optional Cargo features, `wayland-rs` runs by default in pure Rust, and `smallvil`
      is a complete compositor in **1,373 lines**. Caprock already translates Rust for a
      target of its own — Cargo crates against **141 C libraries** is not the same kind
      of work. Reasoning and counter-argument: `~/Dokumente/Caprock OS/COMPOSITOR-WAHL.md`.
      **Hyprland remains stage 2** (Aquamarine backend, 264–887 lines of precedent) — when the question
      changes from "does it run" to "is it usable".

- [ ] **G4. Mesa.** Large, but **userland** — i.e. the good kind of large: it lives in a PD and
      is no TCB. Needs dmabuf semantics (→ P3), shm (→ P2), `dlopen` and a cache path
      (→ P4).

### G5. What the demo path does NOT need — my assessment, not measured

`TODO.md` names three unconditional preconditions (**`CAP_IRQ`**, **large/contiguous DMA**,
**firmware from a file system**). They apply to a **real GPU on bare metal**. For the first
picture under QEMU, in my assessment:

| Precondition | needed for G1–G3? | Reason |
|---|---|---|
| `CAP_IRQ` | **no** | `virtio-gpu`/`-input` are pollable, exactly as `blk`/`net` are today |
| large DMA | **no** | a framebuffer in QEMU can be covered with existing means |
| firmware | **no** | `virtio-gpu` loads none |

**That is an assessment and belongs to be measured before it is planned on.** As soon as real metal
or acceleration is added, all three apply again unchanged — then `CAP_IRQ` is for a
GPU, by its own register statement, "not viable" without.

---

## 7b. Stage D — the own desktop (scope decided 2026-08-17)

**Scope:** shows windows · taskbar · KDE-like appearance · mini programs like KDE ·
**no** file manager and the like · **the mini programs run independently of the desktop** ·
Wayland as protocol. Full derivation with all measured values:
`~/Dokumente/Caprock OS/EIGENER-DESKTOP.md`.

**The blueprint exists and is measured: COSMIC** (System76, Rust, on smithay) — cosmic-comp
⬛ 68,537 · cosmic-panel ⬛ 19,212 · 15 standalone applet crates ⬛ 20,830 · libcosmic ⬛ 60,289.
Licences GPL-3.0-only resp. MPL-2.0, **compatible with AGPL**.

- [ ] **D1. Bar via `wlr_layer`, taskbar via `foreign_toplevel_list`.**
      Both protocols are ⬛ **present** in smithay — taskbar and bar are problems solved at
      protocol level, what is missing is the drawing.
      **Acceptance:** a second window appears **in the list**, not only on the screen.

- [ ] **D2. A mini program as its own PD.** ⬛ Model `cosmic-applet-time`: **887 lines**
      (battery 2,082, audio 2,131).
      **Acceptance:** it ticks — **and it has no cap to disk or network**, proved by
      an attempt that fails.

- [ ] **D3. The line that justifies the whole scope.** An applet is deliberately brought to
      crash; bar, compositor and all other applets **demonstrably** continue running
      — at the round counter, not by eyeball.
      **That is the test that KDE structurally cannot pass:** plasmoids are QML **in**
      `plasmashell`. `cosmic-panel`, in contrast, itself creates a `wayland_server::Display`
      (⬛ `main.rs:315`) — the bar is a nested compositor, every applet its own
      process. **On Caprock that is not a way of building, but the requirement.**

- [ ] **D4. Appearance.** ⬜ The cheapest item of the list — colours, fonts, corners, arrangement
      are a theme file with `iced`/`libcosmic`. Expensive are window management, input and
      frame clock, and nobody sees those.

**Cost frame ⬜** (derived from the models, expressly an estimate): compositor
10,000–20,000 · backend **250–900** (⬛ precedent) · bar+taskbar 3,000–8,000 · per applet
1,000–2,000 · toolbox **0 written**. **Order of magnitude written myself:
15,000–30,000 lines of Rust** — against ⬛ kwin's **356,922**.

**What is expressly missing and why that carries:** no file manager, no settings app, no
terminal, no browser. ⬛ 227 processes and 530 packages of a KDE session are almost entirely
**applications**, not desktop. Whoever leaves them out leaves out 95 % of the work and keeps the statement.

---

## 8. Stage L — the Linux driver layer (independent; only for bare metal)

- [ ] **L1. Assessment before building.** Genode's `dde_linux` is the precedent — proven, with
      Intel graphics. **But GPLv2**, hence friction with AGPL: a layer of our own would be necessary, no
      adopting. Z20 says the decisive thing: *"Whoever wants the desktop does not build i915, but
      `dde_linux`"* — the layer is the investment, the drivers are cheap afterwards, and it
      unlocks **all** (WiFi, GPU, audio, USB).

      **What the layer must at least carry:** `kmalloc`/`vmalloc`, the DMA API
      (`dma_alloc_coherent` → Caprock's IOVA window, **structurally fitting**), IRQ registration
      (→ needs `CAP_IRQ`), workqueues/tasklets/softirq, sleeping **and** spin locks, timers,
      the PCI probe model.

      **Acceptance of the assessment** (not of the build): a **measured** line count for exactly one
      target driver and the list of subsystems it hangs on — not an estimate from
      memory. The same form as the Z26 measurement that turned the expectation.

---

## 9. The TCB statement that must be delivered along

**"Only Caprock as TCB" is false at the moment a compositor runs — and not
because of Caprock.** A compositor sees the pixels of *every* window. Hyprland is six-figure many
lines. No kernel changes that.

The viable version is finer and stronger:

> **The TCB differs per PROPERTY.**
> *Isolation between PDs* → the kernel alone.
> *Availability of graphics* → kernel + driver PD.
> *Confidentiality of the screen* → kernel + compositor.
>
> **A crashing graphics driver takes down its PD and not the system** — with Linux a
> GPU driver error is a kernel error. That is the real advantage, and it remains.

`TODO.md`/Z14 already carries this sentence for the Linux tenant (*"whoever carries 'small TCB' as a
product promise must deliver this sentence along, otherwise it is dishonest"*). **For the
compositor it does not stand there yet and belongs to be added.**

---

## 10. Thresholds that stand IN ADVANCE

According to the house rule: a threshold after the measurement is no threshold. These stand before the
first run and may be changed only with an entry.

| Quantity | Threshold | Reason |
|---|---|---|
| redirected `getpid` | **≤ 2000 cycles** | already stands in the register (anchored at Z18) — only taken over here, not invented anew |
| thread switch in the same PD (K1) | **≤ cost of a PD switch** | a switch without an address-space switch must not be more expensive than one with |
| frame rate G2 (soft, 1280×720) | **≥ 10 fps** | lower bound, not goal — it separates "runs" from "stands", and without a lower bound **zero is green** |
| distinct syscalls that Hyprland calls (P1) | **none** — the number **is** the result | **measured 2026-08-17: 84** (main process), 119 (whole session). Abort threshold 150 **not broken** |

**Every one-sidedly compared quantity needs a plausibility lower bound** — or zero must
expressly drop out as "not measured". That is the F1 lesson and applies here to every row.

---

## 11. Abort and reversal conditions

* ~~**G0 refuted**~~ — **done 2026-08-17: confirmed**, with a counter-proof on a second tree.
* ~~**P1 measures > ~150 distinct syscalls**~~ — **done 2026-08-17: 84 measured.** The
  scope "compositor instead of Linux personality" withstands its own check.
* ~~**M6**~~ — **run 2026-08-17, PASSED.** Build with `--no-default-features --features
  wayland_frontend,desktop` runs through in 17 s: **62 crates, 0 undefined `wl_*` symbols
  (libwayland is not linked), 0 `dlopen`, 0 C compiler calls, 7 undefined C symbols**
  (`ceil floor memcmp memcpy memmove memset raise`), **`unsafe` 461 → 53 in the built part (11 %)**.
  Counter-proof: with `renderer_pixman` C is back in the tree — the feature boundary really separates.
  Thus stage 1 rests on a build instead of a reading. **M7 remains open** (pure
  Rust renderer; `pixman` binds C).
* **The shell grows instead of shrinking**: the "61 instead of 141" are estimated. If they stay above
  ~100, the scope is a distribution project and no port.
* **K1 grows beyond ~800 lines of kernel**: then the TCB promise is touched and the
  decision belongs before the build, not after.
* **No abort because of effort alone.** Every stage is useful on its own (§3) — if the strand ends
  after K or P, nothing is lost that does not stand on the way to the server OS anyway.

---

## 12. What this plan HAS in open questions — and does not hide

- [x] ~~**G0 is unchecked**~~ — **run 2026-08-17, confirmed** (16 methods; 264/246 lines
      from two independent trees).
- [ ] **§7/G5 is an assessment**, not a measurement. `CAP_IRQ` could after all be necessary for smooth input
      — polling on mouse pointer movement is a latency question, and I have not
      measured latency.
- [ ] **The measurement series has uncovered three items that were missing from this plan** (details in
      `~/Dokumente/Caprock OS/KOMPONENTEN.md`):
      **(a) text and fonts** — 14 unavoidable libraries (freetype, harfbuzz, pango, cairo,
      ICU …), the second-largest block after libc, and it occurs in **no** previous
      Caprock planning;
      **(b) the toolchain** — building 141 C libraries means meson + cmake + autotools +
      `pkg-config` + `dlopen`, one build type of its own per library;
      **(c) `xkbcommon`** — without a keyboard layout no mapping scancode→character, and it
      drops out for no backend.
- [ ] **Side finding with consequences for Z26: the machine has TWO GPUs** (`Intel Raptor Lake-S UHD`
      next to the `NVIDIA GB206M`). Z26 is planned entirely around the NVIDIA. For
      display/compositing the **Intel iGPU** is the cheaper way — Z26 itself names i915 with
      728 of 900 files MIT. It does not change the decision, but it does change the **starting point**.
- [ ] **The line counts are missing entirely.** This plan names **no** effort numbers except
      those taken over from `TODO.md`. That is intentional: invented numbers in a plan
      are more expensive than missing ones, because they are later read as measured.
- [ ] **The compositor TCB sentence belongs in `TODO.md`/Z14** (§9) — here it stands, there it is missing.

---

*Created 2026-08-17. This file is a plan and not a register. As soon as an item is begun,
it moves to `TODO.md`.*

## 13. Stage T — NATIVE Caprock drivers instead of Linux ones (decided 2026-08-17)

**The instruction: write native drivers for the local hardware — Ethernet, WLAN, USB, graphics —
rewriting rather than porting, and adapting only where recompiling does not suffice.**

Two measurements were taken before planning, and together they overturn the obvious order.

### The hardware that is actually here (2026-08-17, `lspci -nn`)

| Class | Device | Linux driver |
|---|---|---|
| Ethernet | Realtek RTL8111/8168 `[10ec:8168]` | `r8169` |
| WLAN | MediaTek **MT7925 802.11be** `[14c3:7925]` | `mt7925e` |
| Graphics | Intel Raptor Lake-S UHD `[8086:a78b]` **and** NVIDIA **GB206M / RTX 5070** `[10de:2d18]` | `i915`, `nvidia` |
| USB | Intel 700-series **xHCI** `[8086:7a60]` | `xhci_hcd` |

### What QEMU can emulate — i.e. what is testable before metal boots Caprock

`e1000`, **`e1000e` (82574L)**, **`igb` (82576)**, `rtl8139`, `tulip`, `pcnet`, `vmxnet3`,
`virtio-net` · **`qemu-xhci` / `nec-usb-xhci`** · `bochs-display`, `ramfb`, `VGA`, `virtio-gpu`.

**Not emulable: RTL8168, MT7925, Intel Xe, NVIDIA Blackwell.**

### The verdict, per class — and it differs per class

| Class | Native writable? | Testable before metal? | Call |
|---|---|---|---|
| **Ethernet** | yes, medium | **only for a chip QEMU has** | **do it — but pick `igb`/`e1000e`, not RTL8168** |
| **USB (xHCI)** | yes, large but bounded — the spec is **public** | **yes** (`qemu-xhci`) | do it, after Ethernet |
| **Graphics: scanout** | yes, small (GOP framebuffer) | **yes** (`bochs-display`/`ramfb`) | do it — and it is *enough for a desktop* |
| **Graphics: 3D** | **no** — Xe and Blackwell, both firmware-gated | no | an honest **never**, not a "later" |
| **WLAN** | **effectively no** — signed firmware, an 802.11be MLME stack, regulatory, crypto | **no emulation exists** | keep the contained Linux-driver PD |

### The premise that has to be corrected first

> *"…oder passe an, wenn auf Caprock kompilieren nicht reicht"*

For **drivers, recompiling was never an option at all** — not "not quite enough". A Linux driver is
not a program that makes syscalls; it is a plugin into `net_device`/NAPI/`sk_buff`, or into
DRM/GEM/TTM. The graphics measurement of 2026-08-17 put a number on it (**461 DRM ioctls**, and no
compatibility layer emulates DRM); the network side has the same shape. Recompiling is the path for
**applications**, and it was never the path for drivers.

So the real choice per device is only ever: **native driver**, or **contained Linux driver in a PD**
— and the second is exactly what §0b already blesses (*"das Unbewiesene kann nicht ausbrechen"*).

### Why Ethernet first, and why not the Realtek in this machine

The socket already exists and is measured: A-5.1 put a driver **outside the kernel** as a
replaceable service, A-5.3 moved device assignment into the **manifest**, A-5.4 measured that one
driver PD's device cannot reach the other's DMA region, and `crates/caprock-virtio` already carries
a working `net` driver. A native NIC driver is **slotted into an existing socket**, not the socket.

But a driver for the RTL8168 in this laptop **cannot be tested at all** until metal boots Caprock —
QEMU has no model for it. `igb`/`e1000e` can be developed and regression-tested in the suite *and*
exist on real server boards (i210/i350/X550 are the igb/igc family). That is the same rule that
deferred CAT/MPAM: *buildable but not verifiable is not a starting point.* The Realtek driver is the
**second** one, written once the shape is proven, and it will be the first thing that needs metal.

### Order

- [ ] **T1 — native `igb`/`e1000e` NIC driver as a PD.** Ring setup, descriptors, MMIO, link state,
      RX/TX. Reuses the DMA-region and manifest-assignment machinery unchanged. Testable in the
      suite from day one against `-device igb`.
- [ ] **T2 — scanout.** `bochs-display`/`ramfb` under QEMU, EFI GOP on metal. No GPU driver, no 3D
      — a linear framebuffer is enough for the compositor of stage D, and it removes the single
      biggest dependency of the graphics strand.
- [ ] **T3 — xHCI.** Public specification, emulated by QEMU. Brings HID (keyboard/mouse — which the
      desktop needs) and mass storage.
- [ ] **T4 — RTL8168.** After T1, and the first driver that can only be finished on metal.
- [ ] **WLAN stays contained.** Not a native driver. If wireless has to be native later, the honest
      route is a USB dongle with a documented chip **after T3**, not MT7925.

### The threshold that decides whether T1 was worth it

`igb` must move real frames through a driver PD with the **IOMMU on**, with the same evidence
A-5.2/A-5.4 demanded of virtio: a receive path that proves the device *read* our memory (not just
wrote to it), a counter that moves, and a negative case where the device is denied and the transfer
demonstrably does not happen. A NIC that "initialises" is not a NIC that works — *jede gezählte
Einheit muss eine Arbeit nachweisen, die nur ein laufender Träger leisten kann.*

### 13a. The reordering: T2 comes BEFORE T1 — measured 2026-08-17

**Question asked: what would booting Caprock on real metal verify?**
**Measured answer: today, nothing — the run would be silent, and would probably not reach ACPI.**

Four independent facts, each sufficient on its own:

| # | Fact | Where |
|---|---|---|
| 1 | The machine is **UEFI 64-bit** (efivars present); a modern Lenovo with an RTX 5070 has no CSM to fall back on | `/sys/firmware/efi` |
| 2 | Caprock boots via **Multiboot 1** — a BIOS-era protocol | `.multiboot` section, `MultibootInfo` |
| 3 | The RSDP is found by **scanning the legacy BIOS area** (EBDA pointer at `0x40E`, then `0xE0000..0x100000`). Under UEFI the RSDP lives in the EFI configuration table; those addresses are typically empty | `x86_64/acpi.rs:39` |
| 4 | The console is **COM1 port-I/O only** (`0x3F8`) — and this machine has **no UART** behind it (`/sys/class/tty/ttyS0/type = 0`, `iomem_base = 0x0`) | `x86_64/console.rs` |

Fact 4 is the decisive one and it is the project's own rule turned on the project: **a checker that
cannot speak has not tested anything.** Every report line, every `ALL PASS`, the whole `all_done()`
gate — all of it goes out of a serial port that does not physically exist here.

**Therefore T2 (scanout) is not a graphics feature. It is the precondition for every metal
measurement**, and it moves ahead of T1. A framebuffer is the only output path on this machine that
does not itself require a driver we have not written (USB-serial needs xHCI = T3, writing results to
disk needs an NVMe driver — both circular).

- [ ] **T0 — make a metal boot able to SPEAK.** Three pieces, and the second and third fall out of
      the first: a **Multiboot2** header alongside the existing MB1 one (GRUB-EFI boots `multiboot2`
      from UEFI), the **RSDP from the MB2 ACPI tag** instead of the legacy scan, and a
      **framebuffer console** from the MB2 framebuffer tag. Until this exists, no metal run can
      report anything, and nothing below is reachable.

#### What metal verifies that QEMU structurally CANNOT — the actual reason to boot it

Not drivers. The isolation claims:

- **A1 cache colouring — the whole claim is unverified.** Already recorded in `CLAUDE.md`: as a
  guest the host rewrites the colour bits, measured `disjunkt=234` against `gleichfarbig=210`, i.e.
  **no protection at all**. Everything `color : ALL PASS` says today is a statement about
  bookkeeping, not about caches.
- **B-4.5 Prime+Probe is `SKIP` for a structural reason** — an undisturbed chain link cost 0 cycles,
  below the lower bound of 3, because that measures the emulator's instruction count and not a
  cache. On metal it produces real latencies for the first time.
- **Z6/SMT: the channel, not the policy.** `tools/smt-messen.sh` proves a sibling is recognised and
  suppressed; whether that removes a side channel is not observable under QEMU, and this machine has
  2-way SMT.
- **IOMMU with real RMRRs.** q35 has **0**; on real hardware (legacy USB, BMC, graphics) they are
  the normal case, so the RMRR exclusion path has never run against a real one.
- **D13** — whether the leftover deviations are really measurement-stand artifacts (3.2× vCPU
  overcommit) or something in the kernel.

NUMA is **not** on this list: this machine has one node.

### 13b. T0 in progress — the two pure halves are built and host-tested (2026-08-17)

kexec settles the boot problem: **`kexec-tools` 2.0.32 supports `multiboot-x86` and
`multiboot2-x86`** (measured), so Caprock is loaded straight out of a running Linux — no GRUB, no
UEFI stub, no CSM. What kexec does *not* do is discover ACPI or the display. Linux already knows
both, so the launcher passes them on the command line — the same mechanism Linux itself uses for
kexec under EFI (`acpi_rsdp=`).

**Landed, and host-tested (21 tests):**

| Module | What | Tests |
|---|---|---|
| `caprock_hal::bootparams` | `acpi_rsdp=0x…` and `fb=base,w,h,pitch,bpp`, pure over the command line | 11 |
| `caprock_hal::fbtext` | text-grid arithmetic over a linear framebuffer, `&mut [u8]`, font-agnostic | 10 |

Two rules carry them, and both are this project's own findings applied one level down:

- **A malformed value must never become a plausible one.** `0` is a typable framebuffer base and
  the address of the real-mode IVT; `fb=garbage → base 0` would scribble over low memory and call
  it a display. Every field is `Option`, one bad field discards the **whole** parameter (four of
  five fields is not four fifths of a framebuffer, it is a wrong one), and malformed values are
  **counted** — otherwise a launcher typo and an absent parameter are the same observation.
- **`pitch` is read, never recomputed.** On the measured machine `2560 px × 4 B = 10240 = pitch`
  exactly — which is the condition under which the mistake is invisible. Same shape as *„unten
  zuerst war ein Zufall der Groessenrelation"*. The tests therefore run against a framebuffer whose
  pitch is deliberately **wider** than its line, and one test asserts the wrong answer is not
  produced (`assert_ne!(offset(0,1), Some(64))`).

`fbtext` deliberately takes **numbers** rather than `bootparams::Framebuffer`: the host harness
compiles each module as a single file, so a module that imports a sibling is one nobody can test
alone. Same shape as `cache_decode::colors_from(sets, line, PAGE)`.

*(One test of mine was wrong and the code was right: it claimed `width=1, pitch=u32::MAX` was
impossible. Absurd is not impossible, and a checker may only refuse the second. Recorded in the
test.)*

- [ ] **T0a — wire it up.** Read the Multiboot command line into `bootparams`, use `acpi_rsdp` when
      the legacy scan fails, and add a framebuffer console behind `konsole` alongside COM1. **Both**
      outputs, not one: under QEMU the serial port is the only thing the suites can read, so
      replacing it would make every existing measurement unreadable.
- [ ] **T0b — a bitmap font.** `fbtext` is font-agnostic by design; it needs one 8×16 table.
- [ ] **T0c — `tools/kexec-caprock.sh`.** Assembles the command line from Linux's own knowledge
      (RSDP from `/sys/firmware/efi/systab`, framebuffer from the EFI GOP region) and **fails
      loudly** if it cannot determine a value. A launcher that guesses a framebuffer address is a
      launcher that writes into whatever is there.
      **Open question, and it is the risky one:** with `i915` bound, `fb0` is a DRM framebuffer in
      GTT-mapped stolen memory, not a linear physical one. The reliable source is the **EFI GOP**
      region (`efifb`) — so the first metal attempt boots Linux with `nomodeset`, or reads the GOP
      base out of the EFI memory map. This must be settled before anything is written to it.
- [ ] **T0d — NVMe as the result store.** `/dev/nvme0n1` exists here and **QEMU emulates `-device
      nvme`**, so it is testable in the suite first — the same rule that picked `igb` over RTL8168.
      NVMe is the most tractable modern storage interface (admin queue + one I/O queue, public
      spec). Results go to a **designated LBA range outside every partition**, guarded by a magic
      and read back independently — exactly the Z4 checkpoint pattern (sector 32710), and for the
      same reason: **a write to the wrong LBA destroys the developer's disk.** No write path lands
      before the negative test that proves it refuses an unguarded target.
- [ ] **T0e — the hardware test suite.** Only reachable once T0a–T0c work. What it must measure is
      already listed in §13a: A1 colouring on real caches (today the claim is entirely unverified),
      `pprobe` with real latencies instead of `SKIP`, the SMT *channel* rather than the policy, the
      IOMMU against real RMRRs (q35 has 0), and whether D13 is the measurement stand or the kernel.

---

## 14. Strand P — PostgreSQL on Caprock (measured 2026-08-21)

**The measurement corrected the estimate, and it corrected it downwards where it counts.** Source
clone at `~/Dokumente/Caprock OS/quellen/postgres` (PostgreSQL 20devel, built here against glibc so
the inventory below is read rather than guessed).

### 14.1 Postgres is the SMALL half — with numbers

`fork()` is the one structural obstacle: Caprock has none (0 hits in the tree), and should not get
one — it duplicates address space *and* cspace implicitly, which is the operation a capability
system cannot name. seL4, Fuchsia and Capsicum all refuse it.

**Postgres solved this itself.** `src/include/pg_config_manual.h:122`:

> If EXEC_BACKEND is defined, the postmaster uses an alternative method for starting subprocesses:
> Instead of simply using fork() … **This must be enabled on Windows (because there is no fork()).**

And the seams are *named files*, not scattered logic:

| seam | Unix | Windows |
|---|---|---|
| shared memory | `sysv_shmem.c` 993 | `win32_shmem.c` 650 |
| semaphores | `posix_sema.c` 381 | `win32_sema.c` 234 |

**All four together: 2258 lines.** `EXEC_BACKEND` touches 50 files, concentrated in
`launch_backend.c` and `syslogger.c` (13 hits each). The platform header template is
`win32_port.h` (589 lines) plus 16 `win32*.c` replacements in `src/port/` (63 files total).

So a Caprock port is **a third entry beside `win32` and `sysv`**: an estimated 2500–4000 lines
against 1 208 303 lines in `src/backend`+`src/common`+`src/port` — **0.3 %**. Weeks, not months.

### 14.2 The libc surface is an INVENTORY, not an estimate

`nm -D --undefined-only` over the built `postgres` + `initdb`: **277 symbols.**

| group | n | note |
|---|---|---|
| file/syscall | **45** | the real item |
| math | 28 | libm — portable or vendored |
| string/mem | 25 | trivial |
| process/signal | 22 | includes `fork`, which `EXEC_BACKEND` removes |
| stdio | 20 | |
| glibc internals `__*` | 18 | disappear with musl or an own libc |
| network | 17 | milestone 3, not 1 |
| time | 11 | |
| locale | 9 | a **decision**, see 14.4 |
| **pthread** | **5** | `sem_*` only — **not one `pthread_*`** |
| dl | 4 | avoidable: build extensions statically |

**Postgres brings its own concurrency entirely.** That was the number most likely to sink the
port, and it is the smallest one.

Nothing exotic in the remainder: `getopt_long`, `syslog`, `backtrace` (diagnostics, droppable),
`getpwuid`/`getgrnam` (a stub on Caprock), `tcgetattr` (initdb's terminal only), `syncfs`.

### 14.3 What Postgres needs from the filesystem

`fd.c` + `md.c` measured 17 operations. That figure is a **floor, not a ceiling** — it is the
storage-manager path, and `xlog.c`/`xlogrecovery.c` (WAL segments, `pg_control`), `initdb` (the
directory tree), `postmaster.pid` with lock semantics, tablespaces (`symlink`), `pg_stat` files and
the syslogger all go past it. The `nm` inventory above supersedes the grep: **45 file/syscall
symbols** is the number to design the interface against.

Caprock today has none of them: the fs PD does `INFO/READ/WRITE/FLUSH/SCAN` on **sectors**, and
`caprock-fat` is read-only — no `mkdir`, no `rename`, no `create`.

### 14.4 Two decisions that must fall BEFORE the first line of port code

- **Collation.** Build against ICU or against C collation only, and write it down. Otherwise B-tree
  ordering hangs on our `strcoll`, and a later fix there **silently invalidates existing indexes**.
- **`wal_sync_method` is pinned, never auto-detected.** The eleven optional `HAVE_*` degrade
  cleanly; the sync-method choice does not — it decides which path the durability measurement
  actually exercises. An auto-detected value means the measurement and production may differ.

### 14.5 Four items that are not in `fd.c` and are therefore easy to miss

Measured present, with size:

| item | where | size |
|---|---|---|
| abnormal backend exit → reset the **whole** shared memory | `CleanupBackend`/`HandleChildCrash` | 20 hits in `postmaster.c` |
| postmaster-death detection in the backend | `PostmasterDeathWatchHandle`/`PostmasterIsAlive` | 7 files |
| `statement_timeout` / `deadlock_timeout` | `timeout.c` | 830 lines |
| shared-region placement | `win32_shmem.c` retry loop against ASLR | 12 hits |

The last one is the instructive one: on Caprock placement **can** be deterministic — but that
belongs in the region cap as a **requirement**, not as an accident of the allocator. Read the
Windows retry loop before writing `caprock_shmem.c`.

The second one has its template in the tree too: on Windows it is an event handle, which is what a
notification-based version looks like.

### 14.6 Milestone 0 is the crash probe — BEFORE libc and filesystem

**Twenty lines of synthetic writer: `write` → `flush`, then a real power cut on target hardware with
NVMe and the volatile write cache ENABLED.** Not QEMU-virtio, not a process kill — those exercise a
different cache.

If the chain does not hold, libc and filesystem are lost person-months, and we find out at the end
instead of at the start. Same displacement as chapter 18.

Postgres deliberately **PANICs** when `fsync` fails (`data_sync_retry` in `fd.c`), because a
swallowed error means silent corruption — the fsyncgate aftermath. So the specification comes with
it, and it is one line:

> **An fsync error is STICKY.** Every subsequent call reports it again, until the file is closed and
> reopened. That is what makes `data_sync_retry` and the PANIC path work — and it makes us better
> than the original at exactly this point, where Linux dropped the error and lost the page.

### 14.7 The named assumption — and it is not a footnote

**`EXEC_BACKEND` has NO CI gate in this tree.** Measured: `.cirrus.yml` is gone (CI is
`.github/workflows/pg-ci.yml`), and `EXEC_BACKEND` appears **zero** times in `meson.build`,
`configure.ac` or `src/tools/ci/`. The only switch is `#if defined(WIN32)` in
`pg_config_manual.h`, or a hand-passed `-DEXEC_BACKEND`.

So: it **is** maintained — through the Windows build, and Windows is a supported platform. But the
**Unix** variant of `EXEC_BACKEND`, which is the one a Caprock port resembles, has no automated
coverage; the header calls it *"only useful for verifying those otherwise Windows-specific code
paths"*.

**We are therefore a co-maintainer of the half nobody builds.** That is not a veto, it is a priced
assumption: if the Unix `EXEC_BACKEND` path breaks in a release, upstream will not notice, and the
report comes from us. Budget for it, or carry a patch.

### 14.8 Order

| # | milestone | what it proves |
|---|---|---|
| **0** | crash probe on real NVMe, write cache on | the durability chain carries a database at all |
| 1 | `initdb` + `postgres --single` (3594 lines, single process, no network) | libc subset + filesystem with the 45 operations + `fsync` |
| 2 | `EXEC_BACKEND` with ONE backend | shared region via a cap, latches via notifications |
| 3 | several backends + local client | the concurrency path |
| 4 | power cut mid-commit, database consistent afterwards | the only line that says the stack can hold a database |

Milestone 1 is roughly half the total effort — and almost all of it is libc and filesystem, **not
Postgres**. Which makes Postgres a good acceptance criterion and a bad strand of its own: both
pieces are needed for every other serious program anyway.
