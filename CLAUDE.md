# Caprock

Capability-based microkernel in Rust, written from scratch — not an seL4 fork.
As of this note: 2026-08-03. Only what has been checked.

## The goal (recorded 2026-08-17)

**Caprock OS is in the end to be written entirely in Gabbro — kernel, drivers, desktop — and
formally verified. For usability, unverified things run alongside: Linux drivers and
binary compatibility.**

The split that makes the goal viable — and it is **per property**, not global:

| | in Gabbro, verified | unverified, confined |
|---|---|---|
| **what** | kernel, isolation mechanics, compositor core, own drivers | Linux drivers in PDs, Linux binaries via the personality, foreign applications |
| **promise** | proved against a specification, under named assumptions | **cannot break out** — IOMMU, caps, PD boundary |

**The defensible form of the promise is not "everything is proved, therefore safe", but:
"the unproved cannot break out."** That is precisely what seL4 proves, it is
checkable, and it is the stronger statement — because "completely safe" would be false even with a
complete proof: a proof holds against a **specification**, under assumptions about
compiler, hardware, DMA and initialisation. A wrong specification is proved along with the rest.

From this follows the order that arranges everything else: **first the confinement, then the
proof.** A Linux driver in an IOMMU-bounded PD already carries the promise today; a
proof about it will never exist.

The elaboration with measured values is in `TODO.md` (strand plan graphics/userland) and in
`~/Dokumente/Caprock OS/` (measurement series of 2026-08-17).

## LANGUAGE RULE — everything new is English (decided 2026-08-17)

**All new content is written in English. This applies even when the surrounding file is German.**
A new section inside `TODO.md` is English. A new comment in a German-commented module is English.
The repository becomes bilingual **by date**, which it already is through its commit history —
that is the intended outcome, not an accident.

Scope, so nobody has to guess:

| new | language |
|---|---|
| identifiers (`fn`, `struct`, `const`, `static`, test names) | **English** |
| comments and doc-comments | **English** |
| report lines, log output, error strings | **English** |
| shell/Python scripts: comments and output | **English** |
| new documents, and **new sections inside existing German documents** | **English** |
| commit messages | **English** |

### THE ONE EXCEPTION — and it is a safety rule, not a style rule

**Never rename an existing German identifier as a drive-by.** Measured 2026-08-17: the tree holds
**286** German declarations with **1 879** occurrences, and **40 of them are grepped by name from
`tools/*.sh` and `tools/*.py`** — among them `MELDESTELLEN`, `ERLAUBTE_SPAETBINDUNGEN`,
`MANGEL_VERGIFTET`, `SpaetbindungsGrund`, `bindung_vorhanden_heisst_niemals_kernel`.

> **A renamed identifier whose guard does not move with it finds nothing — and a guard that finds
> nothing looks exactly like a passing test.** This repository has already paid for that failure
> mode (`pdbind : FAILURES` while the suite reported `== ALL PASS ==`).

So: **new declarations are English; references to existing German declarations stay German.**
A deliberate rename is allowed only as its own atomic change — identifier and guard together —
followed by **the counter-proofs, not the suite**: every mutation that failed before must fail
again afterwards. If one suddenly passes, its guard went blind during the rename.

### What has been translated, and what has not (updated 2026-10-19)

On 2026-10-19 the owner decided to translate the **key documents**, and in a second step the
project's memory files. Translated, by hand against the German original and checked mechanically
(digit runs, headings, table rows, code spans): `README.md`, `README-X86.md`, this file (including
the trap register), `LICENSE-EXCEPTION.md`, `TODO.md`, `AGENTS.md`, `STATUS.md`,
`docs/00-overview.md`, `docs/grenze.md`, `docs/plan-betriebsbereit.md`, `docs/verification.md`,
20 of the 23 ADRs (all except 0012, 0014, 0015), `programs/libcaprock/README.md` and six
`tools/lx_*.md` notes (in `tools/lx_schablonen.md` the machine-read contract table stays German on
purpose, because `tools/lx_symbols.py` parses it).

**`TODO.md` is the merge of the former `todo.md` (Part I) and `TODO0.md` (Part II). `done.md` was
removed** — it was not translated, because it went away; it is in the git history (present up to and
including commit `e9d7054`). Entries that say "see `done.md`" keep that wording on purpose.

**Not translated:** `docs/invariants.md` (partly English already), `todo-A-ausfuehren.md`,
`todo-B-verlaesslichkeit.md`, the other `docs/` files, the phase reports, the `Verification/` READMEs
and the German comments in code. Their precision is their value, and a translation yields usable
English and a **blunter claim**. Where a translated document and its German origin (see git history)
ever differ in strength, the German original is the reference.

Inside translated documents, code spans, identifiers, file names and quoted log or report lines
were left as they were — they are grepped by name from tools, see the exception above. The only
deviations are a handful of pseudo-code placeholders in code spans (e.g. `x < bound`,
`grep -q PATTERN`) and decimal commas turned into points; none of them is an identifier.

**Gabbro is unaffected:** decision E1 stands — English keywords, German prose.

## Where the truth lies

This file is an entry aid, not a source. For everything of substance:

| File | Contents |
|---|---|
| `TODO.md` | **exclusively open items** — the source for "what is still to do". Part I (formerly `todo.md`) is the register, Part II (formerly `TODO0.md`) the Linux userland/graphics strand plan |
| git history | finished work: `done.md` was removed on 2026-10-19, its last revision is in commit `e9d7054` |
| `docs/invariants.md` | the load-bearing assurances, among them §2a–2e on DMA |
| `docs/00-overview.md` | structure |
| `README-X86.md` | x86 specifics |

Do **not answer open items from memory**. Read `TODO.md`.

## Current state

Branch `arch/x86_64` (2026-08-03).

**Measured, not remembered:**

| | |
|---|---|
| x86_64 | **49,996 of 50,000** after the D0 fix (2026-08-07 evening, `tools/d0-messen.sh`, 16 parallel streams against ONE reference). **0 D0 hits** — before that 9 in 50,000. The 4 deviations are load artefacts of the measurement rig (3.2-fold vCPU overbooking), see todo D13 |
| x86_64 RAM series | `== ALL PASS ==` at **512M · 2560M · 3G · 6G**, main **and** load suite (2026-08-04). Before E-rest 3 the boot died from 3G on with `#PF cr2=0x70_0000_0014` — the branch "RAM above 4 GiB" had never run |
| K1a/K1b | **`arena : ALL PASS` on both architectures and in both x86 suites** (2026-08-26). The first run of `SYS_SPAWN` at all: five EL0 children from two caps, four of them in windows of ONE arena, `threads=6 slots=2`. Counter-proofs `tools/spawnarena-negativ.sh` |
| cap budget | **`budget : ALL PASS`, `Vorrat 80000 -> 79992 -> 79980 -> 80000`** (2026-08-26) — one account per PD, with return on teardown. The EXHAUSTION PATH has **not** been driven (it needs thousands of PDs), see todo A3 |
| x86_64 load suite | `== ALL PASS ==` (2026-08-03: 39 checks — 5 modules, **two** driver PDs, exchange, A-5.3/A-5.4, plus **three** chained boots for Z4 stage 2, including seven negative cases) |
| aarch64 | **`RUNS=6` → 6 of 6 with identical signature, `== ALL PASS ==`** (2026-08-13, after the C9e fix). Over **12** runs of the same state: **11 clean, 1 D13** — the rate belongs to the number, and D13 is distinguishable here because the colour line itself stands at `ALL PASS` and only the `COLOR_DONE` store comes too late. Before that **ten days red** (`color : FAILURES` with all zeros, 3 of 3), without anyone seeing it: the acceptance series **built** aarch64 and never **booted** it. Since 2026-08-13 `tools/abnahme.sh` runs the suite too — **62 s per run**, measured, against roughly 950 s for the rest of the series |
| host tests | `mem · part · fat · cycles · loader · cap · virtio · typestate · ipctreue` → `== HOST-TESTS: ALL PASS ==` (`tools/host-tests.sh`, 2026-08-03) |
| Verus | **16 proof files, 0 errors** — plus **three** model-fidelity guards (cap_space, IPC, scheduler) with 28 · 28 · 30 self-test cases (2026-08-03) |
| scheduler measurement | `tools/sched-erschoepfung-messen.sh`: 208 measured values, positive control passed, four versions (real/V0/H-a/H-b) — proves D8, D9 and D10 |

**D0 was caught and fixed on 2026-08-07 — the acceptance stands under a caveat.**
0 hits in 50,000 runs against 9 in 50,000 before. **But:** between the two series the
memory controller was corrected, so the parallelism was a different one, and in a start race
the load is exactly the quantity that produces the rate. `P(0 | unchanged) ≈ 1.2·10⁻⁴` thus stands
for "fixed OR less pressure" — the two are not separated. Worse: the condition of the
discovery measurement can **no longer be established** from its log (cut off in the middle).
Since then `tools/d0-messen.sh` takes a fixed number of workers and **names the condition in the
summary**.

In addition: `pdbind` counts **3** bindings on x86, **70** on aarch64 — 50,000 x86 runs cover
three admission sites and leave seventy at the unobserved end. The measurement rig therefore now also runs
`ARCH=arm`. Details and the aarch64 classification in `done.md`.

The cause and the rework are in `done.md`; what remains here is what the error looked like:

**The find itself — with 50,000 runs.** 9 deviations, all nine
character-identical (same MD5 over the signature diff, spread over five streams). Rate **0.0180 %**,
95 % interval **[0.0082 %, 0.0342 %]**, one per **5556** runs.

**With that, yesterday's reading is also settled.** The 2300 clean runs were no fix,
but a sample that was too small: at 0.018 % `0.99982²³⁰⁰ ≈ 66 %` — a null finding was the
**most probable** outcome. The old baseline of 0.5 % (2/400) never held.
Whoever had concluded "fixed" from "2300 green" would have taken 66 % chance for a proof.

**The failure picture is not the presumed hang.** The node runs through the full 61 s
(`ticks=6104` against 52 in the reference, worker rounds 4182 against 27) and passes every other
check. Blocked is **one** thread: the IPC client waits for a reply that never comes, because
the server has left its `RECV`/`REPLY` loop (`IPC-Rolle-abgewiesen=false`). The reason
for the exit fell to the floor until 2026-08-07 — an `if m.result != OK { break }` without storing it.
Since then `IPC_SERVER_EXIT` holds it and the report prints it. Details in `TODO.md` D0.

A caveat that belongs to the number: `pprobe` reports `SKIP` under KVM as a matter of principle
(`CPUID.1:ECX[31]`) and does not judge in this series.

New in the measurement is the **cross-comparison**: until then every run compared only against the *first
run of its own stream*. Five streams, each with a self-consistent but mutually
different signature, would have reported green.

Earlier this stood here as `RUNS=100` → 96 of 100. This number was worthless for a different reason than it
looked: three layers concealed one another (unconditional `SELFTEST COMPLETE`, `color` printed
`FAIL` instead of `FAILURES`, `-no-shutdown` made `rc=124` always true). Only after all three
were gone could a measurement say anything at all — see todo D0.

An earlier "aarch64 66/66, x86_64 24/24" standing here was a snapshot without a date. Numbers
in this file need a state **and a sample size**, otherwise they silently become
false.

## What was added on 2026-08-04

* **D11 fixed: the overflow of an endpoint queue is NAMED.** New ABI code
  `ERR_EP_FULL = 9`; `TidQueue::enqueue` returns `bool` and is `#[must_use]`. `call`/`recv` reject
  **without blocking**, `bind_receiver` and `migrate_owner` report failure instead of success,
  and `migrate_owner` checks **before** the `take()` — the reply duty stays with the old owner,
  instead of being deleted and losing the caller.
  **A fifth site that the finding did not name:** `Notification::wait` had the same
  form at capacity 1 — a second `WAIT` overwrote the waiter.
  The Verus model **followed the code** (`dropped_*` -> `rejected_*`, `send_gate`/`recv_gate`
  with code 3, **25 -> 30 proofs**), the model-fidelity guard runs 99 cases / 35
  self-test cases and keeps a **ledger of the stranded**; the positive control is five
  mutations that restore D11 individually. In the kernel: check line `epfull`.
  Remarkable: the old proof `send_drops_above_cap` **proved the loss**. It was right —
  the code was wrong. A proof that is faithful to the code can prove the wrong thing.
* **E-rest 3b fixed: the free list knows the zone wish.** `alloc_below`/`alloc_colored_below`
  take an upper bound (colour and zone in ONE decision); the three sites with a named
  GiB-0 condition now search, instead of asking once and giving up. The workaround in the memory plan
  is gone — high memory goes **completely** into the free list (with `-m 3G` previously 0 of
  1024 MiB).
  **The finding was bigger than the entry:** "bottom first" was altogether a **coincidence of the
  size relation**. Best fit takes the smallest fitting fragment; as long as the upper area
  happened to be larger (4G, 6G), everything unnamed landed at the bottom. At 3G this reverses,
  and the whole load path fails. "Bottom first" is now a stated policy.
  Two own errors in the process, both in the trap register below.
* **E-rest 3d half done: the allocation sites are ENUMERATED, and memory above
  4 GiB carries, measured.** The classification stands as `enum Zone` in ONE place:
  `KernelOnly` (kernel tables, kernel stacks, segments **and** stacks of loaded programs, all
  L3 tables, AP stacks) prefers **above** — at 3G/6G all 28 of these allocations lie
  above 4 GiB; `PdMappable` must be low, and that is structural (`vspace_map_page_at` rejects
  `va >= GIB1_END`); three sites carry a **hard** condition.
  **The counter-proof is the evidence:** `system::alloc` set to `KernelOnly` tears the
  **load suite** at 3G — while the **main suite stays green**. The same form as D8/D9/D11.
  In the process two own conjectures refuted: loaded segments do **not** need GiB 0 (VA and PA
  are separate), and the device does reach memory above 4 GiB.
* **The GiB-0 cap for isolated PDs is GONE — and it was a number: 504.** The private
  region of an isolated PD is no longer mapped identically, but into a **VA window
  outside the identity map** (x86 `PML4[1]`, aarch64 `L1[9]` — areas in which the
  kernel never accesses identically; otherwise a user VA would cover its own view of physical RAM).
  Proved: `isohigh : ALL PASS` at 3G/4G/6G, regions at 4.04 GiB, colour separation
  unchanged — the colour condition lies on the **phys**ical address. `SKIP` at 512M/2560M, because
  the question is undecidable there. Counter-proof driven.
  Two own assumptions refuted: the 2 MiB block fastpath is **not** lost (it hung on the
  alignment of the VA, not on the identity), and A1/B-4.1 is not affected at all.
* **VA == PA: the BINDING, not just the list.** The cause was a **free argument**
  (`Va::identity(reason, pa)`) — nothing prevented `Va::identity(Mmio, dma_pa)`, and the guard
  would have seen a valid reason. Now there is one constructor **per site**
  (`Va::for_*`), no confusable argument, and the choke points take the constructor as a
  function value. In addition `IdentityClass::{Invariant, Debt}`: "the identity IS the assurance"
  and "fixable, someone should" stood indistinguishably next to each other — the debt would have become
  invisible. `IDENTITY_DEBTS = 3` is a **ratchet** and may only fall.
* **VA == PA now stands in the TYPE.** `addr::Va` has **no** constructor from `u64` or `Pa` —
  the only way is `Va::identity(reason, pa)` with a variant of the closed enum
  `IdentityReason`. The list thus IS the source text. The first version was a script with a
  list in its head and refuted itself on the same day: `vspace_map_dma` was not in it at all, and the
  reason text for `SYS_MAP` was **false** (`sys::MAP` carries no address argument —
  the caller names a cap, the base comes from the cap resolution in the kernel; the identity
  is thus fixable without touching the ABI). **A guard checks the existence of a reason,
  never its truth** — therefore the reasons now carry, where possible, a **falsifier**.
  The new guard immediately found a site that the old one did not see (`unmap_dma_from_thread`).
* **VA == PA, first version: a LIST, not a habit** (superseded, see above) Inventory after the window rework:
  nine call sites in eight identically mapping HAL functions, in three classes.
  **Removed:** `spawn_isolated_native` mapped code and stack identically and took the
  physical address of the code frame as the **entry address** — both now go into the window,
  `vspace_map_region`/`vspace_map_code_region` are without callers and deleted.
  **Made impossible:** `Scheduler::spawn_user` (one value for EL0 SP AND reap region) is
  **deleted**, not repaired; only `spawn_user_at` remains. The last caller writes
  both values although they are equal there — the equality is a coincidence of the environment.
  **Named:** the remaining nine stand with reason in `tools/identitaet.sh` (SYS_MAP/SYS_UNMAP is
  the ABI, device windows are physical, two global mappings have no subject).
  The guard holds the list against the source, with a self-test in **both** directions.
* **Measured:** RAM series 512M · 2560M · 3G · 4G · 6G (main suite) and 512M · 3G · 6G
  (load suite), all `== ALL PASS ==`; x86 `RUNS=8` with identical signature; host tests, Verus,
  three model-fidelity guards, kernel boundary, identity guard, typestate green.
  **aarch64 with a caveat:** 11 green runs (`RUNS=6` identical signature + 5 single runs),
  **one** failure under threefold parallel QEMU load. Fits the open hang from D6 and
  also occurred before these changes — I did not separate it.

## What was added on 2026-08-26 (K1a/K1b + the budget as an account)

* **`SYS_SPAWN` has been RUN for the first time.** The syscall had stood completely in the ABI since 2026-08-17
  and had **no caller and no gate** — zero hits in the whole tree outside
  its definition. Since today `arena` measures it in every run of both x86 suites and on aarch64:
  a parent thread in EL0 creates five children via the ABI, one with the whole cap and four in
  **windows of the same arena**. `arena : ALL PASS (… threads=6 slots=2 …)`.
* **The load-bearing number is `slots=2` next to `threads=6`.** Four running threads only prove that
  four threads run; the point is what they cost. Until then a `Memory` cap bought
  exactly one thread — "how many threads may a PD have" was "how many cap slots are still
  free", and a driver with 6 of 8 occupied slots got to two. In doing so a cap per stack
  buys **no isolation**: threads of a PD share the address space anyway.
* **The finding that is bigger than the entry:** the `Overlaps` rejection was **structurally
  unreachable** for exactly the threads that `SYS_SPAWN` creates. `pd_mapping_overlaps` reads
  `KSTACKS.ubase_of`, and the spawn path deliberately enters nothing there. Two correct
  decisions, together a hole.
* **The cap budget is an ACCOUNT** (`Vorrat 80000 → 79992 → 79980 → 80000`), with `SYS_LOAD` `MSG3`
  as the source and the **return on teardown** as the half that decides whether it is one.
* **And the real cap is `NCAPS = 16`, not the budget.** `CAP_BUDGET_MAX` stood at 64; the
  local cspace of a PD is an array of 16 slots, and `install_cap` rejects above that, **without
  asking the budget**. The self-test found it on the day of introduction. For a
  driver environment with thirty caps a variable cspace is needed — TODO.md Part II K1c.
* **The badge has received its speak check**, prompted by the model-fidelity guard, which complained about
  the rework of 2026-08-25 by itself (`E0061`). On **every** delivery it is now
  checked that the badge of **its** sender stands in the receiver's frame — with two
  positive controls that each reset one delivery path to the old hard `0`.
* **Four own errors, all caught by a measurement, none by proofreading:** the parent thread
  lay in `.text` and faulted at its own entry address (`nm` decided);
  `user_syscall1` put its argument into `MSG0`, while `CDELETE` reads the slot from `x1`; the probe
  read the windows before the children were scheduled; and the budget baseline was taken
  after the first consumption.

## What was added on 2026-08-26 (second part): stage C

* **The DMA pool is an ARGUMENT of loading, not a kernel constant** (`SYS_LOAD` `x5` =
  `(dma_pages << 16) | cap_budget`, both `0` = default). Measured as `dmapool` in the load suite:
  two driver PDs with 8 and 32 pages, `eingeloest`/`verschieden`/`disjunkt`, plus the
  ring-3 negative case from `init`. Beyond `DRIVER_DMA_MAX_PAGES` comes `ERR_DMA_TOO_LARGE` —
  **rejected, not truncated**: a halved DMA region is a device that writes beyond its end.
  The upper bound stands in the **ABI**, not only in the kernel.
* **Two findings while measuring.** The badge sits in the **cap** (my `signal(NTFN_SLOT, BADGE)`
  delivered `ROOT_BADGE`, the line reported `zu-gross-abgewiesen=false` with a correct rejection) —
  stands in the register since A-2.1. And `dma_audit` reports `2` after a hot reload, because two caps
  hold the same region; it therefore stands as a **number** in the line and the more serious question as
  `A3d` in `TODO.md`.
* **Corrected:** the five late probes in `all_done()` **do not gate** — they are measured in the
  report, the gate is the suite. Measured, not inferred.

## What was added on 2026-08-11 (C8: the verifier thread)

* **The crypto no longer stands on the caller's stack — and the number is measured.**
  `SYS_LOAD` → `verify_image` → Ed25519 + SHA-2 ran on the 16 KiB EL1 stack of the *calling*
  EL0 thread and filled it to **73.1 %** (11,992 of 16,384 B). Since C8 the path runs on the
  64 KiB stack of a dedicated verifier thread. Load suite, 512M:
  **EL0 kstack 11,992 → 1,312 B (73.1 % → 8.0 %)**, verifier stack **12,328 / 65,536 B (18.8 %)**.
  The main suite stays at 824 B — **that is the control**, it has no archive and never ran
  crypto via `SYS_LOAD`.
* **The `kstack` line measures something ELSE since then**, and it says so itself. Before it was a
  measurement of the load path, now of the **remainder path**. Whoever holds 1312 against 11,992 compares across
  a change of meaning — the redefinition stands in the line, in both suite scripts
  and in `done.md`.
* **The wait reason is a REASON, not a fourth bit** (`BlockReasons::LOAD`, Z24, sixth instance
  of the same class). The waker is exclusively `load_reply`; `resume`/`unpark`/`unblock`/
  `handler_reply` do not remove it, and enqueueing happens only on an empty set.
* **The serialisation is a NAMED DoS channel:** `AUFTRAEGE_MAX = 4`, overflow
  `ERR_LOAD_BUSY = 13`, and the overflower is **not even blocked** — D11 literally.
  Driven, not claimed: check line `verif`, 5 probes against 4 places, `fill level reached
  4/4`, overflower not blocked and demonstrably continued running on the round counter. Two
  counter-proofs, both with **exactly one** open conjunct (`offen waren: verif`).
* **No lost wakeup, and without a second state.** The caller is blocked **before**
  its job becomes visible — both under ONE locking of the job queue. The other way round a
  verifier on another core could have replied before the reason is set. For
  `PARK` a wake token catches this; here the locking does it (`SCHLANGE` R1.5 → `SCHEDS` R2,
  `docs/invariants.md` §1).
* **Three findings that were not in the assignment:** the **placement policy** would have
  migrated silently (a program without manifest affinity lands on `hal::cpu::core_id()` — until C8
  the caller, afterwards the verifier); the derived **endowment caps leak** on the two new
  rejection paths, because the loader does not even get there; and a **marker on bit 63**
  collided with the topmost number field of the result word (`verloren` read as 128, the verdict
  failed, watchdog — with every single field above it green).
* **Pre-existing, not mine:** the **aarch64 kernel does not build on this branch** (three
  `E0425`: `hal::mmu::guard_unmap`/`guard_remap` exist only in the x86 HAL, since the guard-page
  work of 2026-08-10). The aarch64 side of C8 (`verifizierer::starten()` in `main.rs`) is
  thus **unchecked**.

## What was added on 2026-08-07 (second part: A1 + Z11c)

* **A1 now takes effect on the REGULAR path — and the decision stands in the manifest.** A program
  with `POLICY_EXCLUSIVE_STRIPE` is loaded piece by piece from ONE colour stripe (segments, stack,
  page tables, EL0 kernel stack). Proved as `pdcolor : ALL PASS` — 5 pages in 16 of 512
  colours, measured at the **teardown bookkeeping**, not at the load path.
  The old reason why this would not work was half false: "segments come contiguously from
  `mem_alloc`" described the ALLOCATION of the time, not a necessity — mapping
  had long been done page by page.
* **The manifest policy is APPLIED, not just printed** (`ladepol : ALL PASS`).
  `priority`/`core_affinity` observed; `numa_node != 0`, `POLICY_PINNED` and `budget_us != 0`
  **rejected** instead of silently ignored. For `budget_us` this is a question of format: an
  MCS reservation needs budget AND period, the manifest has one number — to make a
  reservation from one number would mean inventing the period.
* **The finding that is bigger than the entry:** the priorities had always stood in the test manifest
  (3/1/2/2/2) and were never honoured — placeholders. Observed, the same allocation TEARS
  the load suite: a **polling** driver at higher priority than its client
  starves the client. A field that is never honoured accumulates unchecked values,
  and the day of honouring is the day they are all wrong.
* **Three own errors in the checker, all measured:** the first counter-proof was NOT FULFILLABLE
  (it demanded more colours from a 2-page PD than a stripe holds — it failed
  regardless of whether the colouring carries); the measurement stood first in `all_done()`, which is
  POLLED; and the priority was read back in the report, where the thread may already be dead.
  The third was caught by the line itself — it reported **SKIP** instead of ALL PASS, because I had
  switched only one of the two load paths.
* **Way partitioning (CAT/MPAM) assessed, not built, and the reason is a measurement:** on the
  development machine it does not exist (no `cat_l3`/`rdt_a` flag, no `resctrl`). Buildable,
  but not checkable. The design point applies nevertheless: colouring separates SETS, CAT separates WAYS —
  both without a common policy is worse than one.
* **Three entries stood open and were done:** Z11b (manifest signed and bound to the image),
  Z11e (`iface_version` enforced), Z11f (negative list in `docs/invariants.md` §13).
  In addition D11 tags before. A register that keeps finished items makes the question "what is open"
  unanswerable — exactly the form that the model-fidelity guard reported on itself.

## What was added on 2026-08-07

* **D0 is caught AND fixed — after ten days and four measurement series.** The cause was none
  of the three hypotheses that stood in the entry: **a thread was runnable before it had its PD.**
  `spawn()` enqueued, `bind_pd()` came afterwards; if the IPC server falls into this window,
  it makes its first `RECV` with an EMPTY cspace, gets `ERR_NOPD` and leaves its loop
  for good. Measured: 9 hits in 50,000 runs (0.0180 %, one per 5556), all nine
  character-identical; afterwards 5 in 6895 with the reporter, **4 of 4 `ERR_NOPD`**.
  Fixed structurally: the scheduler separates `spawn_parked` from `admit`, 61 call sites
  converted, `load_into_pd` too. **Acceptance: 0 hits in 50,000 runs** on the same
  measurement rig that caught the error 9 times before. Details in `done.md`.
* **The four remaining deviations are a finding about the MEASUREMENT RIG.** Two
  `cycles`, two `freeze` -- and what decided was not the number but the FORM: in one of the
  `freeze` failures the **positive control** failed (`laeuft-vorher=false`), measured
  before anything is frozen at all. An error in the thaw path cannot structurally
  cause it. Common to both pictures is a window in **wall-clock time**, and the rig
  runs 16 guests of 4 vCPUs each on 20 cores -- 3.2-fold overbooking; the `cycles` window measured
  a factor of 4.2. Stands as D13 in `TODO.md`.
* **Why nobody saw it earlier — and the number that belongs to it.** The 2300 clean runs
  of 2026-08-03 counted as "the old rate is excluded". At the true rate
  `0.99982²³⁰⁰ ≈ 66 %` — a null finding was the **most probable** outcome. The reliable
  quantity is not the sample but the **expected number of hits**: 0.41 then, 9.0 now.
* **The crack also sat in the production path.** `load_into_pd` bound the PD and installed the
  endowment AFTER making it runnable; this was covered only by a `local_irq_save` — i.e. by
  two conditions that are written down nowhere (core-local ready queue, load balancing off).
  And the knowledge was there: at **one** of 53 sites there had always been a `local_irq_disable()`
  with exactly this reason. A danger that is warded off by hand at one site and not at
  52 is a missing mechanism, not a question of diligence.
* **The guard for it counts the OPPORTUNITY, not the hit** (`pdbind`). At 0.018 %
  a reporter that speaks only on misfortune would be mute in 5555 of 5556 runs. The ORDER,
  on the other hand, is checkable in every run. With speak check, separate `unklar` and **named**
  exceptions (`SpaetbindungsGrund`) instead of an `if tid == ..`.
* **The model-fidelity guard complained about the change by itself** — and in doing so found an
  outdated register entry that nobody else would have noticed (`spawn` stood
  as state-writing, but writes nothing any more).
* **The gap that the counter does NOT see — and the type that closes it.** `spaet == 0` counts
  LATE BINDINGS, not MISSING ADMISSIONS. Since 2026-08-07 `spawn_*_parked` returns a
  **`Parked`**: `#[must_use]`, no `Drop` (otherwise the field could not be moved out in `admit`),
  **no public way to the `ThreadId`**. The type immediately found a fifth site with authority
  after admission that proofreading had overlooked
  (`map_region_into_thread` at three device backends) -- and the obvious mechanical
  conversion took the error along. Guarded by `tools/zulassung.sh` (7 of 7 in the self-test), there
  also the anchor test for `ERLAUBTE_SPAETBINDUNGEN`.
* **OPEN, and serious: the kernel jumps to address 0** — **0.225 % pooled over 4000
  aarch64 runs** (2026-08-08). An EL0 thread faults with `EC=0x20 FAR=0` (instruction abort,
  its PC stood at 0), afterwards the KERNEL takes `EC=0x21` with `ELR=0`. The affected threads
  lie on **reused slots** (generation 1, in the range of the 1024-thread `scale` test).
  **Not caused by the D0 rework** — measured, not conjectured: 3 in 2000 runs on the state BEFORE
  the rework against 6 in 2000 after, `P(>=6 of 9 in one series) = 0.254` one-sided.
  Thereby my hypothesis is also refuted ("the new state *parked* meets the reap path"
  cannot be right if the picture without this state occurs just as often).
  Details in `TODO.md` D15, logs in `docs/befunde/d15/`.
* **The D0 rework produced a regression, and the kernel audit found it itself.**
  Audit code 7 reads "runnable and in no list" -- literally the state of a PARKED
  thread, which could not exist before the fix. Measured in the aarch64 series:
  `scale : FAILURES`, `sched_audit=7`, run went through completely, every other field
  identical. **1 of 600 aarch64 runs -- and 0 of 56,895 x86 runs**, because the window
  between `spawn_parked` and `admit` exists three times per run on x86 and seventy times on aarch64.
  Fixed by `t.admitted` in the condition; the sharpness for D8 remains.
  **The lesson:** a rework that introduces a new state must take along every site that
  JUDGES about states -- not only those that create them. Neither proofreading
  nor the type found it, but a measurement under load on the architecture where the state occurs often.
* **The counter-proof found that the guard gated nothing.** A mutation produced
  `pdbind : FAILURES` -- and the suite reported `== ALL PASS ==`. x86's `all_done()` built a list
  for the REPORT and returned a separate `&&` chain: 21 members against 24 entries,
  `pdcolor`/`ladepol`/`pdbind` gated nothing. Now the list is the verdict, on both branches.
* **The aarch64 watchdog now names what was open.** Until then the header line promised "open
  tests:" and printed the full report -- in which a never-set statement cannot be told apart from a
  passed one. Thus my own aarch64 hang was unclassifiable. Measured afterwards: **9 of 9 deviations
  `offen: color`**, colour lines byte-identical to the reference,
  watchdog fires between print and `COLOR_DONE` store -- **D13**, no parked thread.
* **What Verus does NOT say about this:** the IPC model does not know "thread without PD" (zero occurrences
  of PD binding or `ERR_NOPD`). "16 files, 0 errors" here means only that the existing
  proofs continue to hold -- about the new property it says nothing.
* **The memory measurement of the D0 controller measured the wrong PID** (the subshell instead of QEMU's
  process tree). The value fell below the lower bound, and that one acted **silently**: the log said
  "about 192 MiB per run" — exactly `128 * 3/2`, i.e. the lower bound and not a measured value. Now over the
  whole tree (361 MiB), and instead of the lower bound there stands a **speak check with abort**:
  a lower bound that steps in when the measurement sees nothing makes its failure invisible.

## What was added on 2026-08-03

* **Z4 stage 2 stands: a thread survives a BOOT BOUNDARY.** The same kernel saves
  and restores and **decides itself which** — it reads one sector (32710, outside
  both partitions), checks magic, format version and `kernel_code_hash` and acts accordingly. The
  transport is the existing block service; the core is client, `virtio-blk` remained unchanged.
  Format in `crates/caprock-cap/src/checkpoint.rs` (fixed widths, LE, CRC-32, dependency-free,
  host-tested), and `Image::build` calls `classify_all`: **a non-transferable cap prevents
  the checkpoint before the write.** Proved over three chained boots (244 → 345 → 446, epochs
  1 → 2 → 3) and two negative cases. Details in `done.md`.
* **Four errors in the kernel found, three of them fixed — all four by checkers that did not
  exist yesterday.** D8 (an exhausted thread ran via `unblock` on an empty account, without any
  cap), D9 (five findings in the donee branch, among them: the nested donation `fs → block service →
  driver` left client **and** server hanging permanently), D10 (O(n²) in the timer interrupt,
  reachable via load balancing) — fixed and measured. **D11 open:** the 33rd sender at
  an endpoint hangs forever, `is_quiescent()` reports it as quiet, `audit` and
  `purge_thread` do not see it. Details in `TODO.md`.
  The pattern behind it is more important than the errors: **none was findable via the test suite.**
  The signature of the x86 suite has remained **byte-identical** across all three fixes
  (`e419003d625f`, 500 runs per state) — the suite never triggered any of them.
* **D0 re-measured: 2300 runs, no deviation — and still not closed.** The old rate of
  0.5 % is excluded, 0.1 % is not. More important than the number: **nobody has fixed this
  hang.** It fell below the measurement threshold, was not repaired — and thus also no longer
  debuggable. Details in `TODO.md` D0.
  In the process a hole in the checker was noticed: **every run compared only against the first run of its
  own stream** — five parallel streams, each with its own self-consistent signature, would have reported green
  five times. The cross-comparison is now part of the measurement.
* **All three CI gates (Kani, Loom, Verus) have NEVER run since their creation.** They lay in
  `.gitea/workflows/`; the server is GitLab. Measured via the pipelines API: two pipelines
  in total, both of 2026-05-23, both with GitLab's Auto DevOps default jobs. Since 2026-08-03
  `.gitlab-ci.yml` lies where the server reads — the jobs come into being, but wait for a
  runner. See trap register below.
* **The finding that rebuilt the whole design in the process — and that holds generally:** the
  progress counter under KVM is **reproducible** (measured 133…155 at the same point of the
  boot-up). The first set-up simply compared "stored value == found value"; a
  mutation that READ and REPORTED the value without setting it hit it **exactly**, and the suite
  stayed green. See below in the trap register.

## What was added on 2026-08-02

* **B-6.2: a kernel panic does NOT take the node down — and that is the worse news.**
  Measured: the panic path halts the core **without IRQ masking**, the next timer tick brings it
  back into the scheduler. The node ran on for 61 s and reported `ipc`, `ring3`, `iommu`,
  `dmatok` as ALL PASS — with a demonstrably violated invariant. Four different outcomes
  depending on **where** the panic occurs, among them a silent total failure (panic under the
  MEM lock) and a state that cannot be told from a deadlock from outside.
  Fixed in `docs/fehlerdomaene.md`, normative as `docs/invariants.md` §14.
* **Z4 begun — the two stages that are checkable today.** **Z4a**: `freeze_thread` stops
  at a *nameable* boundary (not running on a core **and** no open IPC relationship), checked
  via the **effect** — counter moves, stands, runs again. **Z4b**: the refusal rule
  (`crates/caprock-cap/src/checkpoint.rs`, host-tested) — what over there cannot designate the same
  thing does not travel along, and the decision needs the **scope** of the checkpoint, not just
  the cap. Three errors in the own design measured, among them an observation window shorter than
  a tick and an `all_done()` conjunct that only arises in the report.
* **A-5.4 is closed: the device of one driver PD does not reach the DMA region of the other**
  (`dmaiso : ALL PASS`). Four numbers, none suffices alone — positive control over the same
  driver and the same descriptor chain (only **one** address moves), no data on the
  foreign attempt, the victim re-checked **by the kernel**, and a VT-d fault as active evidence. Two
  errors in the own design found: `arp_probe` zeroed only eight bytes, so the second probe read
  the reply of the first; and `rx_used` says that the device *acted*, not that data
  arrived.
* **In the process: two driver PDs run simultaneously**, each gets its device **named** in the manifest.
  The way there exposed four hidden policies — "the first used allocation", "the
  last loaded service", a shared notification store and a shared
  transfer surface. All four are now keyed via the `program_id` from the manifest;
  where a choice would be ambiguous, it is **rejected instead of guessed**. The client names its service
  in the manifest (`service_id`, the last 4 reserved bytes).
* **`pprobe` judged under emulation.** On aarch64 under load the positive control "carried" and the
  distributions were "separable" — at **0.6 cycles per chain link**. A dependent load operation
  costs on real hardware at least the L1 hit latency; below that the test measures the
  instruction count of the emulation, not the cache. Now a lower bound that is independent of the
  measurement.
* **A1 now also applies on aarch64 — and was not detached there but wired to `true`.**
  `spawn_demo` set `COLOR_OK`/`STRIPE_ALLOC_OK`/`PPROBE_OK` hard to `true`: three
  permanently true conjuncts in `all_done()`, no report line, no check — the absence
  was not merely unproved, it was **invisible**. The solution was the **place** (at the end of the chain,
  behind `cross`/`strand`/`loadstop`), not leaving it out. Thus A1 runs for the first time on the
  **16-colour** split — exactly the case that the `MASK_BITS` mix-up got wrong.
* **B-4.5 reproducible — and the cause was NOT the allocator.** The set-up was
  byte-identical (same physical addresses, fragment peak 419/1024); the scatter lay in the
  **measurement**: the attacker ran linearly (positive control 3/10 → bit-reversed 10/10), the
  minimum was the wrong estimator (4/10 → median 10/10), and "victim > L2" was a fixed number.
  In addition: **QEMU reported an invented cache geometry.** With `host-cache-info=on` the kernel sees
  512 real colours instead of 256 imagined ones — and 512 is not 256, that is exactly what the
  A1 residual error hung on.
* **A-5.3: the device assignment stood in the enumerator, now in the manifest.** The entry carries
  a selector (`vendor`/`device`/`class`) in the reserved bytes — format stays frozen, and
  zeros mean "any", not "matches nothing". Fail-closed: if none matches, there is none.
  Proved by two negative cases; the stronger one points to the **network card** and gets it, although the
  block device stands in the offer list before it.
* **B-5.1: the accounting hung on the tick — and entirely.** Not "up to 10 ms distortion":
  `block_current`, `switch_to` and YIELD charged **nothing at all**. Whoever blocks shortly before
  the tick paid zero. Now every rescheduling is stamped (`crates/caprock-sched/src/cycles.rs`,
  dependency-free, host-checked), checked as `cycacct`: **probes > ticks** is the statement.
* **D5: the aarch64 kernel has a root task** — and the way there found two errors that had nothing to do with
  the manifest: `boot_arg` passed the **archive** size instead of the start set
  (now `StartSetNotPrefix`, fail-closed), and `loadstop` measured a **global** baseline with a
  **local** IRQ lock.
* **B-5.5: it was the checker that was bounded, not `revoke`.** `cdt_audit` code 9, peaks as
  operation count in the report.
* **`tools/host-tests.sh`** exists — `caprock-cap` had no host test path at all, its
  six tests would have run nowhere.

## What was added on 2026-08-01

* **D0 half closed.** The colour race is gone (500/500): the kernel side is no longer
  *read back*, but delivered by the spawn — a value that hangs on the liveness of a thread
  that is allowed to die is not fit as a measured quantity. The hang remains open (see above).
* **A real leak fixed on the side:** `record_user_kstack` ran *behind* the critical
  section; if the collector hit the window, the kernel stack was never freed.
* **A-5.2 is closed:** virtio **transport**, **block device** and **network card** on x86, all three
  core-free in `crates/caprock-virtio`. More important than the devices is what they prove: the RNG
  showed only that a device **writes** into our memory; `blk` sends a descriptor chain
  whose first link the device must **read**, and thereby proves the other direction. Also
  in the negative test: after the VT-d set-up the status byte stays at `0xff`, the device has not
  even seen the request header. Details in `done.md`.
* **B-4.5 (Prime+Probe) stands** — with a negative result, see below.
* **The kernel boundary is checkable:** `tools/kernel-grenze.sh`.
* **A-5.1 is closed (2026-08-02): a driver runs as a SERVICE outside the kernel — and is
  exchanged without the kernel knowing what it drives.** `programs/hardware/virtio-blk` resolves
  its device on its **own configuration-space page** and then waits in `recv`; the
  kernel is **client**. Between two requests it exchanges the receiver at the running endpoint
  (A-4.1, without receiver gap); the service counter 1 → 2 in the DMA region proves that the
  new version has **inherited** the same region. In `loader::reload_driver` "virtio" does not occur
  — that is the acceptance condition, not a coincidence.
  The objection that fell away on the way: the configuration space is device-wide — true
  for the ECAM window as a whole, not for **one function** (ECAM maps each to 4 KiB, i.e. to
  one page). **Not included:** `CAP_IRQ` — the driver polls, see below. Details in `done.md`.
* **B-3.3 is closed — and the todo note was only half the truth.** Some things were already aggregated;
  **not** aggregated were three sites with teeth: `init()` armed only unit 0 (the
  others stayed with `TE=0` — that means *no translation*, not *blocked*),
  `invalidate_context_cache()` likewise, and `flush_entry`/`slpt_map` made **policy** (`clflush`,
  `SNP`) according to the capabilities of unit 0. In addition new: a speak check per unit.
  Side finding fixed: `detach` returned **silently** on unreadable capabilities — a
  failed teardown leaves a translation standing. Now `dma_audit` code 8.
* **B-5.5 is closed — and stood exactly the wrong way round.** What was bounded was precisely the **checker**
  (`audit_cdt`); `revoke`, `move_cap` and `child_count` ran unbounded — at the tenant's request and
  under the CAPS lock. Bound now derived (`slots.len()`), overflow counted and checked as
  `cdt_audit` code 9, peaks as **operation count** in the report.
  Side yield: `caprock-cap` had no host test path at all — `tools/host-tests.sh` now collects
  **62 tests** (mem, part, fat, cap) in one place.
* **B-7.1 is closed — and the note was outdated.** Kani had long covered `sync`; the CI only
  described itself wrongly (`Job: "…(loader parser)"`, actually all four targets) — and from that the
  "finding" had arisen. The **real** gap: the proofs ran with CONCRETE values, i.e. in
  the same order of magnitude as Loom. Five new harnesses take the state **symbolically**
  (2^31 reader counts, u32 ticket overflow); `sync` stands at 8 instead of 3 proofs.
* **B-7.2 is closed — and the copy was not the reason.** Loom now checks the **real**
  `caprock-sync` source (the script copies it unchanged, proofs in the same file), and
  the old copies are deleted. The find in the process: with `core::cell::UnsafeCell` Loom checks only the
  **atomic protocol** — a weakened ordering in the ticket release passed ALL proofs.
  Only with `loom::cell::UnsafeCell` do 2 of 10 fail. Measured myself.
* **D6: the aarch64 suite now has a repeat measurement** (`RUNS`, signature comparison, logs
  on deviation). The apparent sporadic rate of ~30 % was mainly the **mechanics** — pipe
  instead of file, afterwards a shared file for all runs. A real hang remains open and
  is now measurable for the first time.
* **B-3.4 is closed:** `0xFEE0_0000..0xFEF0_0000` is unusable as an IOVA (VT-d reads DMA there as an
  interrupt message and does not translate at all — no fault, no error line, only data that
  arrives nowhere). The window base now lies **structurally** above it. The counter-proof shows:
  the window really lay within it before.
* **A-6 is closed: above the sector lies a storage stack — entirely outside the kernel.**
  **A-6.1** block service (info, read, **write**, flush, range error with its own status);
  **A-6.2** `crates/caprock-part` reads GPT (14/14 host tests, three broken tables rejected
  with distinguishable reasons); **A-6.3** `crates/caprock-fat` + `programs/trusted/fs`
  read a file via GPT → FAT16 → block service → driver (16/16 host tests). Both parsers are
  dependency-free and `forbid(unsafe_code)` — foreign disk bytes are interpreted nowhere
  with kernel privilege. **A-6.4** the PD also WRITES (second cluster, both FAT copies,
  flush, every byte read back), and `tools/checkfat.py` re-reads the image **independently**.
  Details in `done.md`.
* **The load suite was red and no longer is** — and the cause is instructive:
  `test-qemu-x86-load.sh` started `virtio-rng-pci` without `iommu_platform=on`, the device was thus
  transitional, the driver aborted correctly, `virtio` failed, `all_done()` never became true,
  watchdog. Looked like a hang, was a device configuration. **Two suites that set up the same
  device differently are a crack through which exactly this falls.**

## The design principle that holds everything together

**A checker that decides about absence must be able to prove that it is able to speak at all.
An empty run is not a test result.**

This is not a question of style, but anchored in the code: `config_errors()` checks
independently of observation, `evtq_liveness` proves that the event queue could answer,
and the audit codes 6 (IOMMU configuration error) and 7 (hanging isolation)
exist so that silence does not pass as success.

Whoever changes something here may easily water this down by accident. Before every change to a
check path: can this test still fail if the thing checked is broken?

The same applies to acceptance criteria. An early criterion read "the existing tests
stop skipping" — and thereby presupposed that they exist. On x86 they did not exist;
they did not skip, they were missing. That is why the DMA tests now lie architecture-neutral in
`kernel/src/dmatests.rs` and are run by **both** boot-up paths.

## Two axes that must never be mixed

`addr::Pa` and `addr::Iova` are separate types. `DmaRegion::identity` was removed
**deliberately** — there is no longer a convenient way to issue a PA as an IOVA.

IOVA windows lie above `RAM_TOP`, with 2 MiB guard bands, and are not
reused. Above `RAM_TOP` an IOVA can never accidentally be a valid PA —
the separation rests on that.

**The trap here:** a function that computes entirely in `u64` is not an edge but a
hole. That is exactly how `dmagen` once read stage-1 leaves with the PA instead of the IOVA — the
newtypes could not see it, because they did not occur anywhere. Where address arithmetic happens,
the types belong in it.

## Traps this project has already paid for

All fixed. They stand here because the condition behind them continues to hold.

* **SMMUv3 `STE.S1STALLD`** may only be set if `IDR0.STALL_MODEL == 0b10`. Otherwise
  `C_BAD_STE`, and the stream is never translated.
* **SMMUv3 CD** needs the bits `A` (terminate) and `R` (record). Without `R` the
  event queue is **structurally** empty — and an empty buffer looks like "no
  errors".
* **QEMU virtio bypasses the SMMU** as long as `VIRTIO_F_ACCESS_PLATFORM` is missing. The driver
  now demands it; the run needs `iommu_platform=on`.
* **x86 `GCMD` is not a read-modify-write.** A written-back `TE=0` releases DMA.
* **x2APIC**: `EN` and `EXTD` in one write is a forbidden state transition —
  #GP, triple fault, back to the BIOS. Two steps, or not at all. Under TCG this does not
  show, because `qemu64` has no x2APIC.
* **`cpuid` in a hot path** is an unconditional VM exit under KVM. In `cycles()`
  it cost 3556 instead of 51 cycles. Features are determined once and cached.
* **Colouring only works on bare metal.** Measured: as a guest the host rewrites the colour bits (they
  lie above the page offset, the second translation stage destroys them). `disjunkt=234`
  against `gleichfarbig=210` — no protection. Whoever runs "isolation without VMs" on rented VMs
  does not have it.
* **An "arch-neutral" barrier is not one.** `core::sync::atomic::fence(SeqCst)` becomes `dmb ish`
  on aarch64 — device memory does not lie in this domain, there `dsb sy` is needed. When decoupling
  `caprock-virtio` that would have been the convenient way and would have silently weakened the
  semantics.
* **A test that occupies memory tips baseline-sensitive tests.** The colour test at the start of
  `threads::spawn_demo` made `captest` fail on aarch64, sometimes `sched`. It is detached there
  therefore (x86 runs it). A test that tips other tests makes the WHOLE
  result unusable.
* **`MASK_BITS` is not the number of colours.** `region_bytes()` computed with 64 instead of with `count()` —
  on x86 (256 colours) coincidentally right, on aarch64 (16) wrong. `caprock_mem::stripe` had
  the same error; **fixed on 2026-08-02**, and it was worse than thought: with 16 colours
  stripe 0 got ALL colours and stripes 1..3 NONE — and because empty sets do not intersect, the
  self-test reported "disjoint". Green, without anything having been separated.
  `stripe` now takes the number of colours as a parameter; at 256 colours bit-identical to before.
* **Kernel hand-over**: `CR3` via a 32-bit write access is a truncated pointer as soon as
  the tables lie above 4 GiB. Request page tables with `GFP_DMA32` and check.
* **Shared page directories do not tolerate PD-specific entries.** `vspace_create_base`
  hangs GiB 1..3 of **every** isolated x86 PD onto the same static tables (`ISO_PD_HIGH`).
  Entering a device window there would give it to EVERY isolated PD -- silently, because the
  cap check would run through correctly. Since A-5.1 a private copy arises at the first device window;
  on teardown only the private ones are freed (the shared ones are kernel memory).
* **A device that only writes proves only the writing.** `virtio-rng` counted as proof for "the
  DMA path carries" — it never reads anything from us, the read direction did not occur in its test case at all.
  This carried into the negative test: that the VT-d default block also blocks read accesses
  was an ASSUMPTION. `virtio-blk` checks it (A-5.2). The same form as the empty event queue without
  `CD.R` — a statement looks true because the case that could refute it never runs.
* **A writer that confirms its own result confirms nothing.** The
  file-system PD read back after writing and reported success — with the same view with which it
  had written. That it had updated only ONE of the two FAT copies was seen only by an
  **independent** reader (`tools/checkfat.py`, different language, pattern written out again there instead of
  imported).
* **Whoever replaces a version must give it EVERYTHING the old one had.** On hot reload the
  new version lacked an endowment slot; it aborted correctly, and the exchange reported
  `NotReady` — which looks like a timing problem and was a missing cap. An
  endowment list that deviates on replacement from the one at first load is a crack.
* **Roles that report need separate stores.** Root task, driver and client shared a
  notification store; the last loaded PD overwrote it, and the kernel waited
  for a signal at the wrong object.
* **A verdict that stands in `all_done()` must not arise only in the report.** Otherwise it cannot
  trigger the report: the run runs into the watchdog and prints the result anyway.
  In the log that looks like "green, but hung". Happened twice in one day (A-6.1).
* **A test that runs nowhere is not a test.** `caprock-cap` had `#[cfg(test)]` modules and
  no way to run them (`cargo test -p` fails on the enforced custom target). Since
  2026-08-02: `tools/host-tests.sh`.
* **Whoever bounds a loop should first check WHICH one is bounded.** Here it was the checker
  and not the path that a tenant triggers.
* **A label that runs next to the thing creates work that is not needed.** The
  CI job was called "Kani — tier-1 proofs (loader parser)" and in truth ran all four targets. From that
  arose a todo entry about a gap that did not exist — while the real gap (proofs
  with concrete instead of symbolic values) remained unnamed.
* **A checker that RECOMPUTES the checked quantity instead of reading it checks a second
  reality.** `iova_window_clear_of_msi` computed the window position itself and in doing so
  mapped only the strong branch; in the weak one it returned `true`, although the window there is
  `[0, 512 GiB)` and CONTAINS the blocked range. Allocator and checker need ONE source.
* **What does not occur on q35 is thereby not absent, but unchecked.** RMRR coloured the
  device instead of the ACS group. q35 has 0 RMRRs, so the QEMU suite could never show this -- on
  real hardware (legacy USB, BMC, graphics) it is the normal case. Where the emulation does not have
  a property at all, a host test with a synthetic topology belongs.
* **A flat identity map is convenient and makes every stray pointer valid.** When pulling up beyond 4 GiB
  512 GiB with 1-GiB leaves would have been 512 entries -- cheaper than
  the chosen way. But then everything is also present where nothing is: a pointer to 200 GiB
  would hit a valid, writable page instead of a fault. What is mapped is what the
  memory plan covers; above that it is rejected, not guessed.
* **An `if cap { .. }` without `else` discards silently — and the caller does not notice.**
  `TidQueue::enqueue` took 32 senders; the 33rd was blocked ANYWAY, got no
  result code, stood in no structure of the endpoint, was never woken — and
  `is_quiescent()` reported it as QUIET. `audit` said `(false,false)`, `purge_thread` `false`.
  A thread hangs permanently, and EVERY checker reports order. Worse than the
  empty event queue without `CD.R`, because the quiet report additionally would release a hot reload.
  Whoever introduces a capacity must **name** the overflow (return value, own
  error code) — otherwise the bound is not a protection but a hole.
* **A proof that proves the wish form is worse than none.** `send_no_loss` did NOT hold at the
  real endpoint (the capacity bound did not occur in the model), `ep_inv` was held not by
  the type, but by call discipline. Both looked green. The fix was not to prove more, but
  to weaken the model to what HOLDS — and to prove the breach of the strong version
  expressly along with it.
* **An iteration count is a property of the program, a time measurement is not.** In D10
  "how expensive is the refill" could not be answered with a stopwatch on a 20-core machine under
  load. Counted iterations yielded a table that can be compared before/after and across
  four versions — and a speak check (table 32 against 10,000).
* **A bit that carries two reasons makes the waker indeterminable.** In the scheduler
  `blocked` meant at once "paused", "waits in IPC" and "waits for account refill". Whoever
  lifts the block thereby also lifts one whose reason they do not know — and whoever leaves it
  standing leaves a thread lying whose waker has dropped away. Four of the five D9 findings
  hung on that (measured: 0 ticks in 3 periods, `audit() == 0`). The fix was not another
  guard, but a separate bit for the REASON.
* **A pointer to "the" receiver of a donation is wrong as soon as donations nest.**
  `sc_donee` held exactly one slot. The second CALL overwrote it, the inner REPLY deleted
  it — afterwards the middle server waited for a waker that no longer existed. Exactly the
  chain `fs -> block service -> driver` that this project itself runs, and without any
  privilege: two CALLs and one REPLY. A donation is a STACK; `sc_donee` was only the top.
* **The obvious version of a guard can be worse than the error.** D8: `unblock`
  enqueued an EXHAUSTED thread unconditionally. The obvious fix
  `if blocked && !depleted { .. }` however also skips `blocked = false` — the RESUME is
  swallowed, and together with the necessary second guard in the refill the thread starves
  **completely** (measured: 0 ticks with budget instead of 6). Right is the guard INSIDE the
  body. Whoever does not measure a fix with the same sharpness as the error merely swaps it
  for a worse one.
* **A guard that keeps complaining after its own fix is switched off.** The
  B2 staleness message in the scheduler guard fired as soon as `audit` checks `depleted` at all
  — i.e. from the day the finding was fixed, for ever. Afterwards it is silent also at the
  next real case. Such messages belong coupled to the register, not to the
  observation.
* **A gate in the format of the wrong server is not a weak gate, but none.** Kani, Loom
  and Verus lay in `.gitea/workflows/`; the server is GitLab and does not read that. Two
  pipelines in the whole project history, both of 2026-05-23, both with the
  Auto DevOps template. The red Verus proof therefore went unnoticed for 37 days — not because
  nobody looked, but because there was nothing to see. **A CI gate is a gate only when one can
  show a pipeline ID.** And afterwards three distinguishable cases count: no pipeline
  (configuration not found), pipeline with `pending` jobs (no runner), green.
* **A local file in `.git/info/exclude` looks versioned.** `CLAUDE.md` stood there. A
  commit with the title "CLAUDE.md to the state of today" contained exclusively `TODO.md`, and
  every change to it lay on exactly one disk. `.gitignore` is noticed when reading, `.git/info/exclude`
  is not — it is not versioned along and stands in no diff.
* **A concurrency proof without tracked cells checks only the atomics.** Loom did not see a
  weakened memory ordering in the ticket release as long as `data` lay in a
  `core::cell::UnsafeCell` — the publication of the payload was not in the model at all.
  Measured: 0 of 6 against 2 of 6.
* **A suite that runs once does not measure.** The ARM side had no repeat measurement until
  2026-08-02, and its output hung on a **pipe** that was lost at SIGKILL.
  That looked like kernel non-determinism (~30 %, a different check each time) and was the
  mechanics. **Second layer of the same error:** afterwards all runs shared ONE log file —
  then early boot lines occasionally went missing, while the result lines (and thus
  the signature) remained complete.
* **Zeroing only the driver part of a virtqueue is not enough.** `used` belongs to the device — but with a
  **reused** region (driver exchange, A-5.1) the end state of the
  previous version still stands there. The device starts again at 0 after the reset, the new version waits for
  a progress that has already occurred and runs into its poll bound. Looks like a
  silent device. The driver initialises the **whole** queue before it releases it.
* **Two suites that set up the same device differently** are a crack: `test-qemu-x86.sh`
  got `iommu_platform=on`, `test-qemu-x86-load.sh` did not — the load suite ran into the watchdog,
  and it looked like a hang.
* **A local IRQ lock is not a window over a shared quantity.** `loadstop` compared
  global counters with `local_irq_disable()` — one core quiet, seven running. That went well as long as
  nothing else happened; with the root task a foreign `free` (16 KiB) fell right into it and
  reported FAILURES. First establish quiet, then measure — and "not measurable" is not a passed
  test.
* **Two numbers that come from the same hand are not two sources.** `boot_arg` gave the
  root task the archive size instead of the start set; on x86 both agreed, because the same
  script produced archive and manifest. On aarch64 (ten foreign modules in the archive) the
  start set would have been wrong.
* **A reproducible value cannot prove that it was inherited.** Z4 stage 2 first compared
  only "stored progress == found progress". Under KVM the boot-up is
  deterministic: the same kernel reaches 133…155 rounds at the same point, scatter about 20.
  A mutation that **read and reported the value without setting it** hit it exactly (151 against
  151) — green suite, nothing restored. The nonce did not help: it proves the **origin
  of the bytes**, not the **effect** of restoring. Two different questions, the same
  distinction as `rx_used` against "data arrived". The solution was a **growing chain**
  (every run works on for +100 rounds, epoch + 1), so that the inherited value lies structurally
  outside what a run reaches alone.
* **A driver's buffer belongs to the LAST client, not to the statement.** `drv : ALL PASS`
  read the driver PD's data buffer **in the report** and compared it with the disk magic. That went
  well as long as there was exactly one client; with a second one its sector stood there, and the line
  reported `FAILURES` for a driver that had done everything right. A value is **captured
  where the statement holds** — not where it is printed.
* **`match lock() { .. None => lock() }` is a self-deadlock in the rare branch.** The
  guard of the scrutinee lives until the end of the `match`; a second `lock()` in the `None` arm
  blocks on a spinlock that the same thread holds. Built in the fallback path of the
  zone policy (E-rest 3b), measured as a standing load suite. An error in the rare branch looks
  like a hang and not like an error.
* **A counter that counts ATTEMPTS does not answer the question about the EFFECT.** The
  fallback counter of E-rest 3b reported `1x` on a 512 MiB machine on which there is no memory
  above 4 GiB at all — what it had counted was a deliberately oversized request
  that fitted NOWHERE. "There was no room below" and "it was taken above" are two statements;
  the same mix-up as `rx_used` against "data have arrived".
* **The loader reports its own memory as free.** The classic GRUB/Multiboot trap:
  from 0 up to the EBDA counts as free, but there lie BIOS data area, EBDA and the stores of the
  loader. Covered here threefold (only type-1 areas, `base >= 1 MiB` discarded, free list from
  16 MiB, modules cut out) -- **but the Multiboot info structure is NOT cut out**.
  Its protection hung solely on `USER_RAM_MIN`; if someone lowers the constant, it drops away silently.
  Since 2026-08-07 a measured line instead of an assumption.
* **An exit criterion that demands repeatability excludes rare errors by
  definition.** The first D12 exit did that -- and the class at issue (missed
  WFE wakeup, timer/IPI race at 1 in 32) precisely does not reproduce. A criterion belongs to
  the **form of the artefact** (complete output, matching exit code, a check line that deviates in content);
  repeatability belongs in the prioritisation.
* **rustc checks PRODUCIBILITY, not NON-PASSING-ON.** A witness with a private field is
  not constructible outside -- inside its module however arbitrarily, and a
  `pub fn witness()`, a `#[derive(Copy)]` or a public field releases the binding again.
  What the type does not cover, the guard must check.
* **A pattern that only hits the usual formatting checks the style and not the property.**
  The field check was anchored at line start and overlooked
  `pub struct T { pub w: Witness }` in one line.
* **A switch-off for a security property is set and never taken back.**
  `SAMMELLAUF_BEHALTEN=0` is replaced by a rotation: the behaviour stays stable without
  anyone having to switch the property off entirely.
* **A hole in the checker damages the GREEN balance, not only the red one.** "Empty final line
  counted as success" did not lose a failure picture -- it **invented a success**: an
  aborted run was booked green, and because on success it was deleted, it could not be
  recounted how often. Whoever finds such a hole must recompute the green numbers, not only
  lament the failures.
* **A collector may only read `$?`.** Success via text comparison of the final line means: with
  every new suite a formula grows, and every formula is a hole. The key of the
  register is the exit code; if a suite returns 0 on failure, THAT is the error --
  one in the suite. The text is cross-read, not interrogated.
* **A witness needs no ancestor.** "`pub(in path)` requires an ancestor" is true and
  is nevertheless no blocker: a `pub struct Witness(())` in the module of the choke point is
  nameable outside, but not producible -- thereby rustc checks the binding site<->reason,
  and the name table in the guard drops away. One line per site. To have classified it as "design work"
  was the same misclassification as `tail -1`.
* **A cardinal number where a set is meant is a ratchet with a hole.** Twice on
  2026-08-05 in the same tool: the arity check counted calls ("one or two times")
  instead of binding them -- two calls from the WRONG pair cannot be told apart from
  mapping+counterpart. And `IDENTITY_DEBTS` was a `usize`: a ratchet over a number
  takes effect only against growth, not against **exchange** -- and exchange feels like progress
  when rebuilding. Both are now sets of names.
* **A collector that files successes as failures makes its directory unreadable -- and an
  EMPTY final line is no success.** `tools/sammellauf.sh` had both errors within an hour:
  success as "ALL PASS in the final line" (the green guards close differently), and
  a run without a final line (SIGKILL, time limit) counted as green -- **silence as success**, in
  the own tool.
* **A selectable reason is a warning sign, not a structure.** `Va::identity(reason, pa)` closed
  the list of reasons, but not the assignment site<->reason: the wrong reason was still
  typable, and the checker would have waved it through. A constructor per site takes away the
  argument that can be confused.
* **A measurement that CANNOT separate at the expected effect size is expensive wall-clock time.**
  The aarch64 bisect (0/6 before against 1/18 after) gives Fisher p ≈ 1 -- at the same rate
  a 6-run before yields zero failures with 71 %. Calculate before the start, not afterwards.
* **A guard checks the EXISTENCE of a reason, never its TRUTH -- a false reason is
  thereby immortal.** The entry on `SYS_MAP` said "that is the ABI"; in fact
  `sys::MAP` carries no address argument, and the identity only arises in the cap resolution of the
  kernel. The false reason made the item appear unfixable. Remedy: where a reason is
  refutable, a **falsifier** belongs with it -- and it must itself check that its anchor exists,
  otherwise it reads into the void.
* **A list in the check script is a text surface; a list in the type is a condition.** The
  first version of the identity guard kept its function list by hand -- `vspace_map_dma`
  was missing, and thereby it never saw the DMA path of a driver PD. The second reads the functions from
  the HAL and otherwise relies on a type without a second constructor.
* **A failure without a log is a lost failure.** Two failures on 2026-08-04
  (aarch64 under load, x86 load suite at 512M) could not be investigated because
  the collection loop kept only `tail -1`. The suites themselves file a full log on deviation;
  whoever loops over them must do so too. At a rate around 1/20 every lost
  failure costs hours.
* **A parameter that carries two meanings is harmless as long as the two happen to be
  equal.** `Scheduler::spawn_user` took ONE value for the EL0 stack pointer AND the
  reap region that goes back to the allocator at thread death. As long as the private region
  was mapped identically (VA == PA) that was the same number. After the window rework not any more:
  `#PF cr2=0x0000008000000000` in the KERNEL, because the reap path freed a virtual address as
  a physical address. The same form as the `blocked` bit in the scheduler (D9). The counterpart
  `spawn_user_at` had long existed -- the load path has used it since A-2.
* **A user VA window belongs where the kernel NEVER accesses identically.** The kernel
  runs at the syscall in the PD's address space and reaches physical RAM via the
  identity map. A user VA in GiB 0 that points to a different PA covers exactly this
  view -- the kernel would read the PD's memory there instead of its own. Hence `PML4[1]` (x86)
  and `L1[9]` (aarch64) and not "somewhere in GiB 0".
* **A measurement on a broken set-up is not a measurement.** The finding "loaded program
  segments need GiB 0" stood half a day as a fact in the head -- it came from
  a run in which a self-deadlock was stuck at the same time. After its fix it was
  refuted: `vspace_map_page_at` takes VA and PA separately, the physical address is free.
  Whoever measures during a bug hunt must first heal the set-up.
* **A classification that is checked only against the MAIN SUITE checks half of it.** If one
  sets `system::alloc` to "pure kernel memory", the main suite stays green and the
  load suite fails at 3G. The same form as D8/D9/D11: the suite does not trigger the case
  that it seems to cover.
* **"Bottom first" was for years a coincidence of the size relation, not a design.** Best fit takes
  the smallest fitting fragment. As long as the memory area above 4 GiB happened to be larger
  than the lower one, everything unnamed landed at the bottom — and dozens of sites got by without a
  zone wish. At `-m 3G` (1024 against 2032 MiB) the relation reverses, and the whole
  load path fails. Where a property follows from a size relation instead of from the
  structure, it disappears at the next measured value.
* **A thread that is runnable before it has its authority.** `spawn()` enqueued,
  `bind_pd()` came afterwards -- in between a memory allocation, a `CAPS.write()`, a tick. Whoever falls into
  this window makes its first syscall with an EMPTY cspace: not *one* cap is
  invisible, but every one. That was **D0**, ten days long under the name "hang from `sched`",
  and the rate was 0.018 %. There is no order that carries -- `bind_pd` needs the `tid`
  that `spawn` only delivers. The gap can be made smaller, not closed. Since 2026-08-07
  the scheduler therefore separates `spawn_parked` from `admit`.
* **"I have never seen this" is a null finding without a size — and therefore slips through even
  more easily than a number.** In D15 "appeared in NO aarch64 log before the
  rework" stood as an indication in the entry. The logs were a handful of `RUNS=6` runs; at
  0.15 % the expected number of hits in them is **0.02**. "Never seen" was the most probable
  observation, regardless of whether the error was there. The measurement then refuted it (3 in 2000).
  The same fallacy as with D0 on 2026-08-03 -- a second time, in the same session in which it
  was written down as a lesson. A statement about absence needs a
  sample size, even when it comes as a memory.
* **A measurement whose most probable result is "nothing" proves nothing.** The 2300
  clean runs of 2026-08-03 read like an acquittal; at the true rate
  `0.99982²³⁰⁰ ≈ 66 %`. The number that belongs to a null measurement is not
  the sample size, but the **expected number of hits** (0.41 then, 9.0 at 50,000).
* **A speak check belongs on the CHECKED PATH, not on an exception within it.** The first
  draft of the `pdbind` guard asked "has the declared exception fired?" -- on aarch64 there
  is none (it sits in the x86 checkpoint set-up), the line would have failed there although
  everything is right. And on x86 it would have started to fail as soon as someone
  REMOVES the exception, i.e. in response to an improvement. What is asked is "was a PD
  bound at all?".
* **A guard that counts the OPPORTUNITY beats one that counts the hit** -- when the
  hit rate is small. At 0.018 % a reporter that speaks only on misfortune is mute in 5555 of
  5556 runs. The ORDER, on the other hand, is checkable in every run.
* **A lower bound that steps in when the measurement sees nothing makes its failure
  invisible.** The D0 controller measured the RSS of the subshell instead of QEMU's process tree; the value fell
  below the 128 MiB lower bound, and in the log stood "about 192 MiB per run" -- exactly
  `128 * 3/2`. That looked like a measured value and was the lower bound. Now: tree sum, and instead of
  the lower bound a speak check with **abort**.
* **The obvious version of a fix can take the error along.** In the D0 conversion
  the `page_probe` probe with `admit_in_pd` got its three mappings AFTER admission -- it would have
  started running before the pages stood, and would have faulted at P instead of at P+8KiB. Green line,
  different test. (The same form as D8: there the obvious guard made the thread starve
  completely.)
* **A checker that reads the wrong QUANTITY structurally cannot see the error it is built against.**
  The `park` line was meant to prove that `unpark` does NOT wake an IPC waiter (the D9 error).
  For that it read `is_parked` before and after -- on an IPC waiter this bit is however false in
  **both** cases, whether woken or not. The line stood at ALL PASS and would have stayed there
  with the error built in. The counter-proof found it, not proofreading. What is measured now is `blocked` -- the quantity that
  actually changes.
* **A mutation that breaks two things at once proves nothing about the one meant.** The
  first D9 counter-proof replaced `if self.tcbs[s].parked` by `if true` -- thereby the
  wake token was **always deleted immediately** in the same stroke. The suite failed, but at the FIRST
  statement (`marke-wirkt=false`), not at the intended one. The clean version only moves the
  `unblock` call out of the `if`; then exactly one field fails (`ipc-bleibt-liegen=false`) and
  all others stay green. A counter-proof must **isolate**, otherwise it measures the order
  of the checks and not the property.
* **Ring-3 code belongs in `.user_text`, and the error message for that looks like a
  kernel error.** A new ring-3 probe without `#[link_section = ".user_text"]` lands in `.text`,
  is not executable from ring 3 and faults **at its own entry address**. In the log there stood
  only "user thread 0x6 faulted (FAR=0x134930)" and a check line with all `false` -- which looks like
  a broken mechanism and is a missing line. `nm` decided: the address of the
  symbol was the fault address. A helper called along must be `#[inline(always)]`, otherwise
  IT lies in `.text` and the error merely moves one level deeper.
* **`wrapping_sub` on a time difference is the most expensive convenient line.** A counter that
  jumps back by 100 cycles would yield about `2^64` — an account charged like that is immediately and
  permanently exhausted. Backwards means **discarded**, not "almost once around".
* **`echo "$X" | grep -q PATTERN` under `set -o pipefail` reports "not found" as soon as the
  output exceeds the pipe buffer.** `grep -q` exits at the **first hit**, `echo`
  gets SIGPIPE, `pipefail` passes rc=141 through. Measured it tips **between 66 and 70 KiB** —
  thereby the verdict of three QEMU suites hung on the **size of their own output**, and a
  sixth archive entry pushed it over the edge: **nine checks reported FAIL for
  lines that stood in the log**. That is the worse direction of "invented successes": invented
  **failures** cost no failure picture, they **drown** it. Remedy:
  `grep -q PATTERN <<<"$X"` (no pipeline), and a speak check of the checker on a deliberately
  **large** input — on a small one it would have been green throughout the error.
  `bash -n` does not catch this: the broken line is syntactically valid.
* **One store per ROLE is one store too few as soon as there are two instances of the role.**
  "Three roles, three badges, three stores" fixed A-6.3 — and `Client` is a **role**. With the
  second client PD it overwrote the first one's store, a check procedure waited for a badge
  at a foreign object, and three check lines failed **without the thing checked being broken**.
  The comment at the site already described the error literally and did not prevent it:
  the fix lay one level too shallow. Keyed is with the `program_id`, not with
  the role — the same solution as with the four hidden policies from A-5.4.
* **A waker that does not NAME its reason wakes foreign decisions along with it.** `blocked` was
  one bit for four situations (IPC, budget, pause, park); every split repaired the last
  collision and **set up the next**. Since the reason set (Z24) every waker removes
  exactly its reason, and enqueueing happens **only on an empty set** — without this second half-sentence
  the set is merely another notation for the same bits. The rework exposed four sites
  that lived off the muddle: `RESUME` called `unblock`, `thaw_thread` likewise (although
  `freeze` freezes via `pause`), the IPC **fastpath** let a paused target run immediately,
  and a cross-core test woke a *parked* thread with the waker for IPC.
  Hallmark of the class: they are not noticed in proofreading, but in the suite of the architecture
  on which the state occurs often.
* **Whoever changes two quantities at once must not blame the more convenient one.** An
  additional `println!` line tipped the Z4f checkpoint test — reproducibly, gone when taken back.
  I ascribed it to the **output length** (i.e. wall-clock time, matching D13) and entered it that way
  in the register. Wrong: one line changes **output length AND binary**, and the check read
  `d[16] != 0`, i.e. the **first byte of the kernel hash** — the line tested a hash value.
  False alarm at 1 of 256 builds, blind at **255 of 256** real overwrites.
  The assignment "reproduces per binary" would have excluded the time thesis immediately; I had the
  fitting history and did not hold the unfitting observation against it. **Two
  simultaneously changed quantities are two hypotheses, not one.**
* **A checker that reads a single BYTE checks a byte — not the property.** Now compared
  are the 512 bytes themselves (SHA-256 before and after the boot): detection **1 of 256**
  to **256 of 256**. And the balance sequence belongs to it: every previous PASS of this line was
  a statement about a hash byte — whether the kernel ever overwrote the sector is, for the
  past, **unanswered**, not "no".
* **WHOEVER MEASURES SETS THE MARK — the measured path may neither set nor delete it.** That is
  the common root of two errors of the same day: the poisoned shortage mark was **dead**,
  because every `spawn_*` path deleted it in its first statement (between poisoning and the first
  request) — "the path was silent" was structurally unreachable, while the line promised it
  literally; and the `park` line read a quantity that the measured path itself wrote.
  Both stood at ALL PASS. **The structural version is a counter that only grows**
  (`MANGEL_GEN`): the measurer reads before and after, the difference is the statement, and no
  refactoring of the path can devalue it. A reset in the measured path is always a suspicion.
* **A number that a human keeps in parallel to the truth is the same class as a checker that
  recomputes its quantity — even with a ratchet.** `MELDESTELLEN` was maintained by hand and held by
  a guard; the ratchet caught a merge error (31 against 32, because two branches
  counted independently) and was nevertheless **two memories for one fact**. Since
  2026-08-11 `kernel/build.rs` derives the number from **one** counter; after a merge it can
  no longer be wrong. The guard has since watched over the **derivation** — that it stays wired —,
  not over a second number. Fail-closed: if it cannot be counted, the build aborts.
* **Cargo merges `.cargo/config.toml` from EVERY ancestor directory — and APPENDS arrays.**
  A working tree below another checkout (`<repo>/.claude/worktrees/<id>`) inherits the
  configuration a second time; `-Tkernel/x86_64-link.ld` then stands **twice** on the linker line,
  `lld` evaluates `SECTIONS` **twice**, and **all** linker symbols carry the
  values of the second pass (`__text_start = 0x100000`). The build runs through, the image
  never boots. That cost half a day and three fix attempts at the **linker script** —
  a symptom. The main tree was only coincidentally immune (there is one configuration there). Since
  2026-08-10 the script stands in `kernel/build.rs` (`rustc-link-arg`, **exactly once** per crate,
  absolute path) and a guard aborts on a second `-T`. **A de-duplicator would have been the
  wrong answer:** it would have to guess which repetition is intended — discarding a
  legitimately duplicate flag is a silent change of semantics.
* **"`cargo build` runs through" is no evidence as long as nobody binds the CONFIGURATION.** The
  binary fingerprint excludes "stale build" for a *suite run*; for the
  *build environment* the same door stood open. Every build line that serves as evidence prints its effective flags —
  and the kernel carries their fingerprint (`CAPROCK_FLAGS_FP`) in the image.
  A suite that sends its build output to `/dev/null` is the actual crack.
* **Zero is a finding, not a measured value.** A one-sided threshold comparison (`x < bound`) is
  **green as soon as the measurement fails**. `NOSEL_TEXT` stood at 0 because the build was broken, and
  the F1 line reported `PASS` for "0 < 0x62000". The same form as a never-set bit that is read as
  "no error". Every measured quantity with only **one** bound needs a
  plausibility lower bound — or zero must expressly drop out as "not measured".
* **A page that does NOTHING can make an assurance unfulfillable — if it hangs on a
  size relation.** The guard page (`USER_KSTACK_ALLOC = USER_KSTACK_SIZE + PAGE`, since
  2026-08-10) made **every coloured kernel stack request** structurally impossible on aarch64:
  16 colours, 4 partitions → a stripe is **4** colours wide, an EL0 kernel stack
  is 16 KiB = **4** pages there. It fitted *exactly* — the property followed from
  `stack pages == stripe width`, not from the structure. With the guard there are **5**
  consecutive pages, i.e. five **different** colours; those do not fit into a
  4-colour stripe at any address and at any fill level. `alloc_colored` always gave `None`,
  `spawn_isolated_colored` likewise, the A1 colour test reported all zeros — **ten days
  long** (C9e). The punchline: on aarch64 the guard **does not exist at all**
  (`hal::mmu::guard_unterstuetzt() == false`, `guard_unmap` is a counter). A page that
  demonstrably does nothing on this architecture tipped the colour test of the architecture.
  The same class as "bottom first was a coincidence of the size relation". Since then the guard stands
  under **no** colour condition (`alloc_colored_vorspann_in`) — it carries no data and occupies
  no cache set, the assurance says nothing about it.
* **A `None` from an allocator means "no room" — even when it means "does not work in principle".**
  Exactly that is what the C9e search hung on: the failure picture looked like memory shortage under load
  and was an impossibility that occurs just the same with empty RAM. A request that is
  **wider than the stripe** is no shortage; it needs its own reason.
* **A failure BEFORE the `if let` colours exactly the fields zero that arise behind it — and
  that is the diagnosis.** In the `color` line stood `uebergross_abgewiesen=1` amid
  all zeros. This single one decided the question: the test was *able to speak* and ran up to the
  spawn; everything after did not take place. Whoever sees a line of all zeros must first
  ask which fields are set **before** and which **after** the abort point.
* **The same class twice in one day: arch-neutral kernel code calls a HAL function that
  exists only on x86.** On 2026-08-12 at 00:48 `hal::mmu::guard_*` was fixed and the
  acceptance extended by `aarch64-bau` — "the acceptance now catches the class". At 10:24
  `irq_tiefe`/`irq_tiefe_melden` (C4, second summand of the stack computation) landed again **only** in the
  x86 HAL, called from `kernel/src/kstackmark.rs` and `kernel/src/system.rs`: three `E0425`,
  the aarch64 build broken. **A gate that exists and is not run is none** — the
  ratchet takes effect only when it lies between commit and merge. And the obvious fix
  (a second copy in the aarch64 HAL) would have been the wrong one: the content is **two
  atomics**, i.e. arch-neutral. It now stands once in `caprock_hal::irqtiefe`, both
  `exception` modules pass it on.
* **A criterion that CANNOT reach the thing checked is not a strict criterion,
  but none at all.** The FP probe had to "survive all 64 handovers"; reachable were 3, because
  it advances one iteration per round-robin round and a round is bounded by the **tick**,
  not by the `YIELD`. Green was impossible, so red said nothing — the line had no
  discriminating power, and its justification in the report was additionally **outdated** since A4. What is asked
  is the quantity that really changes (progress, corruption report, **own**
  preemption count), not the conveniently formulable one.
* **A scaling curve without a proof of liveness counts corpses.** The isolated series of the
  capacity curve reported 224/1504/3040 "isolated processes" — in fact `kurven_arbeiter`
  lay in `.text`, every EL0 thread faulted immediately at its own entry address (**228
  `el0-trap` lines at 224 PDs**, at the end **0 occupied VSpaces**). What was measured was the
  speed of dying. The condition behind it continues to hold and is more general than the
  entry error: **every counted unit must prove work that only a RUNNING
  carrier can do.** "Created" is no such work — `spawn` gives `Some` even if the
  thread never executes an instruction, and a curve that counts return values rises just
  as nicely then. The corrected series (220/1477) hangs on **occupied VSpaces** and **held
  page-table frames**: quantities that go back as soon as the carriers die. The same
  distinction as `rx_used` against "data have arrived".
* **A mark that the checked path deletes in its first line is no mark.**
  `MANGEL_VERGIFTET = 255` was meant to make "the path was silent" distinguishable from "it was due to no
  resource" — but every `spawn_*` path calls `mangel_zuruecksetzen()` as its **first**
  statement, i.e. between poisoning and the first request. The outcome that
  the report line literally promised ("if it still stands there, the path was SILENT") was thereby
  structurally unreachable. Measured with a muted reporting site: with the fix
  the sweep reports `geschwiegen=5`, without it `keiner=5` — and `keiner` means "was due to no
  resource", exactly what the mark was meant to separate. **Whoever sets a mark must check the
  path it is to survive** — otherwise the checker checks a property that is gone before its
  first measurement. The same form as the empty event queue without `CD.R`.
* **A predicate that reads the checked quantity only by PROXY checks the proxy.**
  The Z4f re-check "the rejected checkpoint remained unchanged" read
  `d[0:8] == b'SL4KCKPT' and d[16] != 0x00` — and `d[16]` is the **first byte of the kernel
  hash** (`OFF_BODY = 16`), exactly the byte that the negative case flips beforehand with `^= 0xFF`.
  Thereby the verdict hung on the SHA-256 of the just-built kernel: **false alarm at 1 of 256
  builds** (`hash[0] == 0xFF` → flipped `0x00`, "overwritten" without anything having been written)
  and **blind spot at 255 of 256** real overwrites. That was D14: "one
  `println!` line more tips the check, not its content, its mere existence" — a
  report line changes **two** quantities, the output length and the binary; only the second is
  deterministic, and the time thesis read the wrong one. The same class as `rx_used` against "data
  have arrived". Now the **512 bytes themselves** are compared, before and after the boot.
  And: such a line damages the **green** balance — every PASS of it was a statement about
  a hash byte, not about the sector.
* **A derived denominator next to hand-kept summands is half a derivation.**
  `system::MELDESTELLEN` has come from a counter since 2026-08-11 — the split of the
  C7 coverage ("11 provoked + 9 room + 10 load path + 1") stood next to it as **prose**. When the
  guard page brought a 32nd reporting site, the denominator went along and the summands did not: 31
  against 32, in the report, unnoticed. Prose has no gate. The summands now stand as
  constants with `const _: () = assert!(… == MELDESTELLEN)` — whoever adds a reporting site
  breaks the build until they have classified it.
* **The guard of a `while let` scrutinee lives across the BODY — and `SpinLock::lock` masks
  IRQs.** `while let Some(a) = SCHLANGE.lock().entnehmen() { laden(&a) }` looks like "take,
  release, work" and is "work under the lock, with locked interrupts". In the
  C8 verifier that would have run the entire Ed25519/SHA-2 check with masked IRQs --
  **i.e. exactly the version "preemption off for the duration" that C8 expressly rejected**,
  brought in by a temporaries lifetime instead of by a decision. Measured with a
  `Drop` witness, not inferred: in the `while let` the guard falls AFTER the body, in
  `let v = { l.lock().entnehmen() };` before. The same root as the `match lock() { .. }`
  self-deadlock above, but with different damage: no hang, but a latency hole that no check line looks at.
  Remedy is the **function boundary**
  (`fn naechster() -> Option<Auftrag>`), not a comment.
* **A status mark in the topmost bit collides with the topmost NUMBER field.** The result word of the
  C8 probe packs statements into the lower and numbers into the upper bits; "measured" stood on bit 63,
  and there already lies `verloren` (bits 56..64). On unpacking the mark read as
  `verloren = 128`, the verdict failed, the run went into the watchdog — **while every single
  field of the line above was green**. The picture cannot be told from a real finding,
  and the cause is a bit mask. Whoever packs numbers and marks into one word must write out the layout,
  not count it off.
* **Whoever MOVES a deep path also moves everything that hangs on "where am I running".**
  `load_into_pd_mit` places a thread without manifest affinity on `hal::cpu::core_id()`. As long as
  the load path ran in the caller, that was the caller's core; with the verifier thread it would have become
  its core — every program loaded without affinity would have changed core, as a
  **side effect of a stack move**, without a line speaking of it. The job therefore carries
  the home core along. The same class as "a parameter that carries two meanings": an
  environment quantity becomes an argument as soon as the environment changes.
* **A new rejection path does NOT inherit the clean-up duty of the old one.** For `SYS_LOAD` the
  dispatch derives endowment caps; on the accepted path `load_by_index` cleans them up on
  failure. The two new exits (`Ausgelastet`, `KeinVerifizierer`) no longer get there at all —
  without their own deletion they remained as orphaned CDT children and even blocked the `delete` of the
  parent cap. Whoever splits a function into "hand over" and "execute" must re-house every
  clean-up promise of the old function individually.
* **A negative test can secure a property that NOBODY USES.** The whole sidecar arithmetic of Z26/A3
  (`slot_offset`, `slot_gueltig`, `slots_in`, `fenster_deckt`,
  `FRAME_MAX_BYTES`) had had, for three days, no caller in the **whole tree** outside
  its own test module — and two mutations in `tools/redirect-negativ.sh` (M5: off-by-one at the
  slot bound, M6: window checked only at the slot start) dutifully proved that the **functions**
  are right. That they are **called** was proved by nothing. That is "a green line that
  gates nothing" one level deeper, and it is harder to notice: a negative test looks like
  the strictest checker in the house. Beside it lay a real gap: the mask width (64 slots) and
  the window length in the cap were **two independent numbers** — a 4 KiB cap would have granted 64 slots
  and the kernel written up to 32 KiB. `grep` for the callers is cheaper than any
  mutation.
* **Reading and writing are NOT the same authority — even if the same region carries both.**
  The authority table of Z26/A3 had a row "read frame **and** write — yes, whole
  frame". The whole frame however contains `cs`/`ss` (x86) resp. `spsr` (aarch64), i.e. the **ring**:
  a handler that were allowed to write them back would promote its guest — the privilege escalation
  that the switch stands against, only from the other side. Therefore the whole frame is read,
  and only the general-purpose registers are written back. Where a table keeps two directions in one row,
  there probably stands the weaker half of a stronger grant.
* **Two suites that set up the same KEY differently** — the same crack form as with
  `iommu_platform=on`, only more expensive. `test-qemu-x86-load.sh` has checked, since 2026-08-01, the
  **agreement** of private and compiled-in public TrustedSAS key
  (`tools/check_trusted_key.py`), with a detailed justification in the script;
  `test-qemu.sh` (aarch64) checked until 2026-08-13 only the **existence** of the private file. A
  `git checkout kernel/src/trusted_keys.rs` — i.e. exactly the tidying that `AGENTS.md` demands at the
  end of the session — then lets the aarch64 suite fail with
  `root : FAILURES (Rejected(Unverified))` plus three follow-up lines. That looks like a
  kernel finding and is a set-up problem. **Whoever sharpens a check in one suite must
  take the other along** — otherwise the case falls exactly where nobody expects it.
* **A convenient query under a lock is a latency hole — and it is noticed at a line that
  has nothing to do with the matter.** `pd_haelt_dma` (Z23/S5) asked per cap slot of a PD via
  `CapSpace::inspect`, and `inspect` computes `child_count`, i.e. a CDT walk with the bound
  `slots.len()` — all under a held `CAPS` lock, and **`SpinLock` masks IRQs**. Measured
  on 2026-08-21: the longest masked stretch on aarch64 rose to **3,701,562** cycles, with the fix
  (`kind_of`, no CDT walk; lock only for the slot copy) **220,081** — factor 17. What turned red
  was the **DEBUGGER's stop-latency promise**, a line without any relation to the freeze, and its report showed
  all `true`: the fallen summand stands in a number, not in a
  conjunct. Two lessons, and the second is the more expensive: an information function that
  **incidentally** makes a walk is at a choke point not a convenience but an error; and whoever
  checks a composite bound must print the **summand**, not only the verdict,
  otherwise the next one searches at the wrong place. The same class as the `while let` guard in the
  C8 verifier -- only that this time a check line looked.
* **A scope that the caller writes down is a CLAIM — and a guard that reads it
  checks the claim and not the thing.** `Scope::endpoints` has carried, since Z4b, the doc
  "endpoints whose **both sides** are part of the checkpoint"; `classify` has
  **believed** the list. A client blocked at a named endpoint that stays behind appeared
  nowhere — the cap was "transferable", the endpoint travelled, and the waiter sat for ever at
  a rendezvous point that does not exist on this machine any more. **It was concealed by
  `Scope::EMPTY`:** with an empty scope `classify` rejects every relationship cap anyway, the
  piece that remained unchecked was thus unreachable from the only call site — and an
  unreachable hole is one that becomes reachable at the first named endpoint. Since Z4d
  stage 1 `Image::build` takes the **observed** edges and holds them against the scope: *the
  channel travels ⇔ the thread that holds a role there travels.* The same family as "a
  guard checks the EXISTENCE of a reason, never its TRUTH" and as `ep_inv`, which was held not by
  the type, but by call discipline.
* **A question that can only be asked "per thread" does not find the participant that nobody
  enumerated.** `quiescence_of(tid)` answers "in which roles does THIS thread stand" — and
  the dangerous participant of a checkpoint is by construction one that the checkpoint never
  named. The opposite direction ("who stands HERE?") did not exist at all until 2026-08-25, and as long as
  it was missing, half the rule could not even be formulated. Whoever makes an assurance about
  a set needs an **enumeration** and not only a membership question.
* **A negative case needs not only "did the pattern hit?", but "HOW OFTEN?".** The
  M7 counter-proof for `ckptcut` replaced `for i in 0..ntfns().len() {` — and this pattern occurs
  **twice** in `system.rs`, the second time in `thread_quiescence`, on which the speak checks of the
  probe itself rest. The mutation thereby blinded the collection **and** its checker: two things
  at once, and that proves nothing about either (the first D9 counter-proof, literally). In the
  **result** it was not visible — the checked conjuncts came out the same in both versions; it was found
  only by a `grep -c`. A `sed` with two hits looks in the result line like one with
  one.
* **A mutation that breaks the BUILD measures the build.** The first version of the M4 counter-proof to
  `ckptcut` replaced two call sites with `Ok(0)` and thereby took the `if` expression its
  type information (`E0282`); the run had no check line at all. What caught it was the question "does the
  conjunct occur at all?" — without it `false` and `not present` are the same in a
  `grep` comparison, and the file would have reported a passed negative case. Every
  negative case needs the existence check next to the value check.
* **Whoever prioritises by REACHABILITY instead of by HISTORY has the tool at the end exactly
  where the last case lay.** The ten unmeasured shortage reporting sites of the load path
  stood for a year with the reason "needs a boot archive that the main suite
  structurally does not have" — while the load suite had the archive. The load path is the one
  on which `NoResources` was silent for six weeks and on which `wasmhost` died.
* **"Built" and "measured" are two states, and a register that mixes them up sends
  the next plan to the wrong place.** `SYS_SPAWN` had stood completely in the ABI since 2026-08-17 —
  six named rejections, `ERR_INUSE`, D0 admission order — and until
  2026-08-26 had **no caller, no report line and no suite gate**: zero hits in the whole
  tree outside its definition and a number anchor. `TODO.md` meanwhile listed it as
  *missing* ("the ABI has no `SPAWN`"), i.e. as something other than *unmeasured*. Both versions
  were wrong, and the second is the more dangerous: against "missing" one plans the build, against
  "unmeasured" the measurement. `grep` for the callers is cheaper than any conjecture — the same
  line as with the sidecar arithmetic, which had no caller for three days.
* **A checker that reads a table in which the checked path DELIBERATELY enters nothing
  cannot structurally see its own case.** `pd_mapping_overlaps` decides the
  `Overlaps` rejection of `SYS_SPAWN` and reads `KSTACKS.ubase_of` for that;
  `spawn_with_stack_parked` writes **with justification** nothing there (the region belongs to the cap,
  not to the kernel). The comment at the function said literally "checked are ... exactly the stacks that a
  second `SPAWN` could hit" — and exactly those it never saw. As long as a cap carried one stack,
  it was not noticed; with several stacks in a cap it is the main case. The same family as
  the empty event queue without `CD.R`, only that here two *correct* decisions
  together yield a hole.
* **A cap above the structure is not a generous cap, but an unfulfillable promise.**
  `CAP_BUDGET_MAX` stood at 64; the local cspace of a PD is an array of
  `NCAPS` = 16 slots, and `install_cap` rejects every slot above that, **without ever asking the budget**.
  A budget of 20 thereby failed at 16 — and the rejection did not even carry the reason "budget".
  The self-test found it on the day of introduction, not proofreading. The same
  form as "a criterion that CANNOT reach the thing checked is no criterion at all"; the
  fix is a `const _: () = assert!` next to the constant, not a comment.
* **A conjunct in `all_done()` that is only measured in the REPORT gates nothing — it merely
  labels.** On 2026-08-25/26 five probes (`dbg`, `pdfreeze`, `ckptcut`, `arena`,
  `dmapool`) were taken into the flag list, on the assumption that they thereby hang on the verdict. But
  they are all measured in `report_and_off`, i.e. **after** `all_done()` has decided.
  Measured instead of inferred: a run with `arena : FAILURES` ended with `SELFTEST COMPLETE` and
  without a watchdog. What gated was the **suite** (`check`, return code 1). The entry effects exactly
  one thing: the line appears in the `offen waren:` enumeration.
  That is the same trap as "a verdict that stands in `all_done()` must not arise only in the report"
  — only without the hang, because `NichtGefahren` does not gate. That is why it is not noticed:
  the run looks healthy, and the line seems to carry. Here the position is
  INTENT (the probes occupy memory and would tip baselines further up); the error was
  the CLAIM next to it. Whoever hooks in a probe should first check in which function it measures.
* **Editing a running bash script changes it IN THE MIDDLE OF THE RUN.** Bash reads the file
  incrementally and remembers a **byte offset**; a change above it shifts it,
  and the interpreter continues in the middle of a token. Measured on 2026-08-26 in
  `tools/spawnarena-negativ.sh`: `Zeile 129: ppfehler.: Kommando nicht gefunden` and
  `Zeile 164: M4:: Kommando nicht gefunden` -- fragments of comment text that did not exist at these
  lines. A block was executed twice, a `sed` then hit nothing and
  reported a "shut-down counter-proof" that in truth merely stood at the wrong place. The
  run is thereby **unusable, but not obviously red** -- the other checks continued to run
  and reported PASS. The same applies to the suite scripts.
* **Two counter-proof runs at the same time mutate the SAME files.** On 2026-08-26 a
  second run started while the first had applied M3; its positive control saw `magisch=1/4` and
  reported "the initial state is already red". That it reported this is the reason why it was noticed
  — without a positive control five mutations would have run against an already mutated
  tree and would have looked like evidence. A tool that mutates the working tree tolerates exactly one instance.
* **"The guard does not know the hand-over" was the CONVENIENT reading — and it was false.**
  `dma_bounds_audit` reports `2` after every A-5.1 hot reload, and the first thought was that its rule
  ("different objects must never overlap") was too strict for an intentional hand-over. The source text
  says the opposite: `for_each_dma` runs **object-** and not
  cap-granular, expressly "so that cap copies do not count the same region several times" — the case
  is thus already exempted. `reassign_driver_device` however **does not copy**, it mints
  a second OBJECT. The 2 tells the truth.
  And the core is not the refcount, but the **IOMMU registration**: as long as the IOVA->PA
  mapping stands, the device writes into the pages, regardless of whom they belong to in the meantime.
  The order that must hold is stop device -> IOMMU unmap -> free;
  the teardown token is built for it, and the reload does not run through it (todo A3d).
  **Two lessons:** whoever sees an unexpected number should first check whether the checker really does
  not know the case — the answer is in the source and not in plausibility. And a number with an
  expected value belongs as an **equality** in the conjunct, not as prose next to it: a number
  whose expectation is 2 is otherwise never checked and is silent also at 3.
* **Whoever changes an ABI must take along every caller — also the one that only the OTHER
  architecture has.** `SYS_LOAD`'s `x3`/`x4` were re-purposed on 2026-08-25 (single slot + badge
  -> packed list + count). The in-kernel caller `sysload_caller` exists **only on aarch64**,
  it still encoded the old version, and aarch64 ran again only on 2026-08-26. The dispatch read
  `anzahl == 0`, delegated **nothing**, `hello` ran without notification — and never reported.
  The failure picture is the expensive kind: `sysload` said `result=0` (the loading succeeded after all) and
  `hello-Signal=NEIN`, `all_done()` never became true, watchdog, and **nine further lines** stood
  at `FAILURES`, because they did not run at all after the hang. Nine red lines from a
  single wrongly encoded message word. The same family as "two suites that set up the same
  device differently" — only that here the second suite was not run for days.
* **A fixed bound next to a table dimensioned at boot time is a second
  memory — and it is noticed on the architecture where the numbers are large.**
  `STACK_CAP_SLOTS = 1024` covered, according to the comment, "the `scale` test (1024 threads)"; the
  thread capacity on aarch64 with eight cores however is about **10,000**, and the slots lie
  correspondingly high. Consequence: **every** `SYS_SPAWN` got `ERR_NOSPACE` there — and indeed *after*
  the thread had been admitted and immediately killed again. On x86 the same ran green, because the slots
  coincidentally stayed small there. That is literally "bottom first was a coincidence of the
  size relation", only with a table length instead of a memory area. The fix is
  **no larger constant**, but the same `total` with which the four neighbouring tables are
  dimensioned. It was found by the aarch64 suite, not proofreading — and only because
  the probe gates on **both** architectures.
* **A fail-closed path conceals its cause when its error code is shared.** The same
  case: `record_stack_cap` gave `false`, `dispatch_spawn` killed the thread and reported
  `ERR_NOSPACE` — the same code as "region overlaps" and "no memory". In the report stood
  `codes: ganz=7 ueber=7 aus=21`, i.e. **three times the same 7 for three different reasons**, and the
  line `ueberlappung-abgewiesen=true` was *accidentally* green: it checks for 7, and 7 came anyway.
  A conjunct that is green for the wrong reason cannot be told from a correct one
  — what decided here was `threads=7` next to `maske=0x0`: seven threads
  in the PD, not a single successful spawn.
* **A baseline taken AFTER the first consumption measures the difference to itself.**
  The first version of the budget line read the stock after the test had already created a PD,
  and compared against it at the end — it reported `FAILURES`, while the printed
  numbers proved the mechanism exactly (`80000 -> 79992 -> 79972 -> 80000`). Two lines further on
  the same error in a different guise: the bound was asked **before** the budget was
  exhausted. A bound is checked at the bound, and a baseline before the first
  consumption.

## Structure, roughly

| Place | Contents |
|---|---|
| `kernel/src/system.rs` | core of the capability management, IOVA windows, teardown token, audit |
| `kernel/src/addr.rs` | `Pa`, `Iova`, `DmaRegion` |
| `kernel/src/dmatests.rs` | architecture-neutral DMA tests, run by both boot-up paths |
| `kernel/src/arch/x86_64/bootinfo.rs` | `HandoverInfo` — one structure, two origins |
| `kernel/src/arch/x86_64/dmar_selftest.rs` | synthetic DMAR for the self-test |
| `crates/caprock-hal/` | `vtd`, `dmar`, `intc`, `timer`, `fault`, `iommu` facade |
| `crates/caprock-cap/src/space.rs` | `Finalized`, CDT, `delete_leaf` |
| `crates/caprock-cap/src/checkpoint.rs` | Z4: the refusal rule (`classify`) **and** the checkpoint format (`Image`, CRC-32). Dependency-free, without `unsafe`, host-tested — a checkpoint is input, not state |
| `crates/caprock-virtio/` | virtio: `Transport` + `Queue`, on top `rng`/`blk`/`net`, plus `probe_ecam`. **Without any dependency** — linked by the driver PD (A-5.1) |
| `programs/hardware/virtio-blk/` | **the first driver outside the kernel** (A-5.1): resolves its device itself, serves requests via its channel, replaceable at run time; since A-6 also block service + GPT scan |
| `crates/caprock-part/` | GPT parser (A-6.2). Dependency-free, `forbid(unsafe_code)`, host-tested — foreign disk bytes do not belong in the kernel |
| `crates/caprock-fat/` | FAT16 parser (A-6.3), likewise |
| `programs/trusted/fs/` | **file-system PD** (A-6.3): drives no device, calls the block service |
| `tools/mkgpt.py` | builds the GPT test images, also **broken** ones (`--break`) — both suites use the same tool |
| `kernel/src/colors.rs` | colour allocation, `run_color`, Prime+Probe (B-4.5) — arch-neutral |
| `crates/caprock-sched/src/cycles.rs` | cycle accounting (B-5.1) — **without any dependency**, so that the traps can be triggered with literals instead of with a machine |
| `tools/kernel-grenze.sh` | checks that no drivers migrate into the HAL; with self-test. **State 2026-08-26: RED and pre-existing** -- `fbtext`, `iommu_health`, `numa`, `bootparams`, `smt` lie in the HAL and stand neither on the allow list nor as a named exception. Neither guard nor HAL has been changed since the last green state; the modules arrived individually, and each needs a decision (userland PD or named exception WITH justification), not a collective admission |
| `kernel/src/spawnarena.rs` | **K1a/K1b: the first run of `SYS_SPAWN` at all** -- five EL0 children from two caps, four of them in windows of ONE arena. Arch-neutral, run by both boot-up paths. Counter-proofs in `tools/spawnarena-negativ.sh` |
| `tools/eingeschlossenheit.py` | **the entry ticket for a handler module (Z28)**: whoever carries `[package.metadata.caprock] einschluss = "streng"` has `forbid(unsafe_code)`, no build script and only named dependencies. Two ratchets as sets of names, 15 speak checks, **0 candidates = return code 3** |
| `tools/host-tests.sh` | the host tests of the pure crates in **one** place (`caprock-cap` ran nowhere before) |
| `tools/handover/` | Linux kernel module for the core hand-over (variant B) |

## If you work here on the server

This copy contains **no `target/`** and no large test data — see `../MEMORY.md`.
A missing build directory is intentional, not a defect. The first build takes correspondingly long.

## The next step

**Strand A-5 is closed.** Two driver PDs, each with its device named in the manifest, and the
separation between them is measured instead of claimed (A-5.4).

Everything else in `TODO.md` — read, do not guess. Two things that follow immediately:

* **`CAP_IRQ` for drivers** needs an **IRTE allocation** — the interrupt remapping table stands
  at all "not present" since B-3.2, and that is intentional. Until then every driver polls. That is
  B-3 work, not A-5.
* **The device OFFER is still virtio-bound**: the kernel finds candidates via
  `hal::pcie::find(VIRTIO_VENDOR, …)`, because the BAR determination goes through `probe_transport`. The
  selection has stood cleanly in the manifest since A-5.3, the offer does not.
