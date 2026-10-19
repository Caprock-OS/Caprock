# STATUS — running state of both strands

*This file is the view from outside: **what is running now, what is finished, what is blocked.**
Both agents write their own section and leave the other's alone.
It is updated after every completed step, not by the clock.*

**Last changed (B): 2026-07-29 19:38 UTC**

---

## Strand B — Reliability and isolation (Claude B)

**Currently in progress:** B-1.2c (repeat measurement anew, with the separated marker). B-1.5 to
B-1.8 are done (see below), B-2 as well.

**B-1.7 answered:** the x86 error does **not** exist on aarch64 — but for a stronger
reason than conjectured. Not "the ARM suite builds an archive", but: the arch-neutral `all_done()`
in `threads/mod.rs` contains **no** archive-dependent statement **at all**. The flip side, found in
the process and reported to A (message 8): precisely for that reason the root task on ARM is in **no**
completion condition and is not checked by grep either — its failure would be invisible.

**B-1.8 — the most unpleasant find:** `report_and_off()` printed `SELFTEST COMPLETE` on x86
**unconditionally**, even after the watchdog (in the same log l. 99 `WATCHDOG`, l. 119 `COMPLETE`).
Thus precisely the marker on which the repeat measurement stands could not tell a complete
run from an aborted one. Separated on the aarch64 model, the suite takes over
the separation. **Consequence: B-1.2s "16 of 16" is not refuted, but not proved** — counted with this
marker. To be measured anew as B-1.2c.

**The find of the day (B-1.6):** the x86 suite has issued its report **every time from the
emergency brake** since `6d68328` — `all_done()` demanded `root_chain_done() && cdelete_done()`, both
in principle unfulfillable without a boot archive, and this suite deliberately builds none. Thus the
report appeared after a spin counter instead of after the last proof, and **every tight statement was a
race**: the `iso` test delivered `2x`, `1x`, `0x` faults with an identical build. Fixed; afterwards
**5 of 5 runs without WATCHDOG, `iso` green throughout**, only FAIL `x2APIC` (TCG).

**Released for A (message 6):** the full x86 suite ran with A's `default = []` in the
working tree — **exactly one `FAIL`, `x2APIC`**, i.e. unchanged compared with 12:38. A-2.2 is
thereby proved from the B side; `kernel/Cargo.toml` and `test-qemu-x86-load.sh` wait for A's commit.
I have not touched them (rule 2). Log: `build/diag/b-uebernahme-suite.log`.

**Next: B-4.2 before B-4.1 — the order is reversed, and for a reason.** Originally
B-4.1 (coloured path as the normal case) stood first. When reading the code two reasons showed up
why that does not work:

* **There are only four stripes.** `PARTITIONS = 4`, and `mask_for(i)` allocates them with `i % 4` —
  round-robin, **without an occupancy check**. As long as the coloured path is the exception (today
  only the colour test calls it, with `i = 0,1`), this is harmless. As the normal case the **fifth**
  simultaneous PD silently shares its colours with the first — and the suite starts isolated
  threads in rows. From "uncoloured, hence no promise" would become "promise given and silently broken".
* **The region shrinks by a factor of 32.** `spawn_isolated` gives 2 MiB (`ISO_REGION_SIZE`), the
  coloured path `region_bytes() = MASK_BITS/PARTITIONS × 4 KiB = 64 KiB`. A turn of the dial would silently
  shorten every isolated thread's user stack until one overflows.

The way for B-4.1 afterwards: assemble the 2 MiB **from several coloured runs of the same stripe**
and map page by page — then region size *and* colour property remain. The price
is the 512 PTEs instead of one block descriptor, and that already stands in the function doc anyway.

**Done and proved:**

| Item | Result | Commit |
|---|---|---|
| B-1.1/1.2 IRQ safety of the SpinLocks on x86 | 7 of 8 → **16 of 16** complete runs | `ab76273` |
| B-1.4 error class searched + guard at compile time | `caprock-sync` was the only affected crate; sensitivity proved | `ab76273` |
| B-1.3 repeat mode of the suite | `RUNS=n`, a rate below 100 % is FAIL; trial run 5 of 5 | `b43fc14` |
| B-2.1 ARM suite from a fresh clone | test key is generated instead of checked in, kernel rebuilt afterwards; **`== ALL PASS ==` from a fresh clone of HEAD** | `02a1407` |
| B-2.2 `hal::cache` really executed on ARM | `cortex-a72`/`a53` → 16 colours, `max` → 32: **the values differ**, so CCSIDR is read and no constant | `d4d27f1` |
| B-2.2b CCIDX branch checked | field decomposition as a pure function (`hal::cache_decode`), both layouts against fed-in register values, **5 of 5** — although no QEMU CPU reports CCIDX | `02a1407` |
| B-2.3 README | from "aarch64, phase 7" to the actual state; two false statements of the draft found and corrected during checking | `529bc35` |
| B-2.4 `docs/verification.md` | Loom stage 2 ticked off — **with the limit beside it** that became expensive on 2026-07-29 | `529bc35` |
| B-4.4 assurance written down honestly | `invariants.md` §12: what A1 separates, and the longer list of what it does **not** | `529bc35` |
| B-1.5 Expected absence stated | two checks demonstrate that without a boot archive no root task runs **and the kernel names the reason** — no filter that hides the lines | `8f5b2a7` |
| B-1.6 Report came from the emergency brake | `all_done()` was unfulfillable without an archive -> WATCHDOG in EVERY run; afterwards **9 of 9 without watchdog**, `iso` green throughout (before 2 of 4) | `8f5b2a7` |
| B-1.7 aarch64 cross-checked | error does **not** exist there — the arch-neutral `all_done()` has no archive-dependent statement at all. Flip side to A: its root task is in NO completion condition on ARM | *this commit* |
| B-1.8 Success marker lied | `SELFTEST COMPLETE` was printed even after the watchdog (l. 99 + l. 119 in the same log); separated on the aarch64 model, suite takes over the separation | *this commit* |
| A1 stage 1 cache colouring | `color : ALL PASS`, 256 colours measured | `7a87182` |
| Feature `selftest` (todo F1) | `.text` 0x25000 → 0x11000 (54 %) | `7a87182` |
| Target architecture Z, plan, strand split | — | `6e4cf9d` |

**Test situation x86 (9 runs, 19:36, after B-1.6/B-1.8, with A's `default = []` in the tree):** **9 of 9 without
WATCHDOG**, `iso` green throughout, only FAIL: `x2APIC` — TCG fundamentally cannot do the
feature (`TCG doesn't support requested feature: CPUID.01H:ECX.x2apic`), no
`/dev/kvm` in the container. **No regression, but a limit of the set-up.**

**Test situation aarch64: runs** (since B-2.1), most recently `== ALL PASS ==` from a fresh clone. Thus
A's new ARM root task path no longer depends on an argument either.

**Host arithmetic (17:15):** `caprock-mem` 13 of 13, `hal::cache_decode` 5 of 5, 0.00 s each.

**Blocked:** nothing.

**Relevant for strand A:** `system::testsupport` now lies behind `selftest`;
`test-qemu-x86.sh` also builds the configuration without the feature and checks that `.text` shrinks
in doing so. Details in [AGENTS.md](AGENTS.md), message 1.

---

## Strand A — Executing and exchanging (Claude A)

**Addendum 2026-10-09 (opencode, branch `ipc-cap`): IPC channel rule + PDCTL creation.**
`CHAN = 38` (channel rule per endpoint cap, send-time check in `CALL`), PDCTL
`CREATE/MAP_INTO/SPAWN_INTO` (process-service primitives in the kernel). Evidence: host tests
`cap`/`cspace` green, kernel builds x86+aarch64, Verus model `verus/cap_ipc_rights.rs`
8/8, QEMU main suite x86 ALL PASS (KVM) + ARM suite ALL PASS (81×). Load suite first run:
4 FAILs (A-1.2, N4, A-5.3×2) — A-5.3 also on a baseline without my commits (foreign/pre-existing);
N4 mechanism afterwards proved by hand on HEAD (manifest accepted, rejection there) —
re-run of the load suite serially (`build/diag/ipc-cap-load2.log`): N4 + A-1.2 PASS —
the first run was due to a mixed build artefact, no code regression. Remaining: A-5.3×2 as on
a baseline without my commits (foreign/pre-existing, see AGENTS.md 21 patch-text offer).

**Last changed (A): 2026-07-31 16:55 UTC**

**Currently in progress: nothing begun — A-4 is COMPLETE.** A-4.3 is committed (`78f0d52`),
thereby A-4.1 (`a25fa23`), A-4.2 (`4fca286`), A-4.3 (`78f0d52`), A-4.4 and A-4.5 stand; A-3.4 is
complete (part 1 `ec26cfb`, part 2 `1e2bd51`, part 3 `f6e5186`, part 4 `25d388a`, sum check
`d1f5ed8`). In the working tree lies only **foreign** work: the QEMU selection block in
`test-qemu-x86.sh` and `todo-B-verlaesslichkeit.md` (both B, not committed along).
**The next step is to be chosen** — A-4 prescribes none any more.

**A-4.3 — state hand-over (`78f0d52`).** What is chosen is the region that survives the exchange —
and thereby its format is an **ABI**. The point on which the matter hangs is not the
survival of the bytes, but the question whether the new version knows *which* bytes it reads: a
region without a header merely shifts the problem, because v2 then interprets v1's data in its own sense.
That is not data loss, but a misinterpreted state — the one is noticed, the
other is not. So a versioned header (`state.rs` in `caprock-region`): `state_version`,
`program_id`, take-over counter; deviating layout and foreign `program_id` are **rejected**
instead of interpreted, a fresh region reports `NoState` instead of "version 0", and the take-over counter
rises only on an actual take-over — if a rejection raised it, it would prove the opposite
of what it claims. Proved in two stages: gate logic on x86 (`state : ALL PASS`),
real case on aarch64 (`ckpt : ALL PASS` — v1 computes `+1` and leaves `[1, 2, 3]`, v2 computes
with `+10` and continues on `[13, 23]`, i.e. on the taken-over values, generation 1). The real case
does **not** run on x86, because it hangs on the arch-neutral thread demo, which does not start there;
that stands as a remark in the x86 script, so that a green `state` no longer promises more than it
checks. Suite run 16:21/16:23: `rc_load=0 rc_arm=0`, both logs `== ALL PASS ==`, no FAIL line.

**Trap after container rebuild (hit twice today):** the Python package `cryptography`
was missing (load suite aborts with `rc_load=2`, manifest signing does not work) and the
**git identity was gone** (`Author identity unknown`). The former via
`pip3 install --user --break-system-packages cryptography`, the latter deliberately **not** written into the
repo config, but set per commit via `GIT_AUTHOR_*`/`GIT_COMMITTER_*` — a
repo-wide identity would let strand B commit under A's name.

**A-4.1 — atomic rebinding (`a25fa23`).** The exchange so far checked and swapped afterwards.
Between the two lies a window: the finding "no open transaction" is a snapshot
that no longer holds in the next cycle; and between "old server detached" and "new one bound"
the endpoint would briefly have **no** receiver — a client that sends exactly then sees a
server that never existed. Now check and swap run under *one* lock. Without prior
quiescing it is rejected, a foreign receiver blocks the swap instead of being silently displaced, and
in the overlapping case exactly one receiver is bound throughout. Proved:
`rebind : ALL PASS`, run 13:04 with `rc_arm=0 rc_load=0 rc_main=0` — both suites without a
single FAIL line.

**A-4.2 — the resting point (`4fca286`).** Built before A-4.1, on purpose: 4.1's honest version
("exchange only without an open transaction") presupposes the concept *quiescent*, which did not exist, and
the same concept later carries Z4a (freeze thread). Quiescing rejects **new** transactions with
`ERR_QUIESCING`, not with `ERR_BADCAP` — "comes back soon" is for the client a different
situation from "does not exist"; whoever mixes the two forces it to give up where waiting would be right.
Running transactions may finish, a second exchange at the same endpoint is
rejected, release opens the gate again, double release is reported. Proved:
`quiesce : ALL PASS`, run 11:30/11:51 with `rc_arm=0 rc_load=0 rc_main=0`.

**The `x2APIC` caveat is gone — and it never lay with the kernel.** Since `/opt/tools/qemu`
(QEMU 11.0.3, installed from outside) the kernel reports `apic: x2APIC (MSR-Pfad)` instead of the
MMIO fallback; Debian's 7.2 simply cannot do the feature under TCG
(`TCG doesn't support requested feature: CPUID.01H:ECX.x2apic`). The main suite is thereby for the first time
`== ALL PASS ==` without exception. **Open imbalance:** `test-qemu-x86-load.sh:103` continues to call the
bare `qemu-system-x86_64` (7.2) — both suites run on **different** machine models,
a green result does not mean the same thing in both. To be brought in line as soon as the foreign QEMU block
is committed.

**Environment trap that cost a run at 11:47:** the container was rebuilt, `~/.local`
was gone, and with it the `cryptography` retrofitted via `pip3 install --user` — without it
no manifest can be signed and the load suite aborts before the first build (`rc_load=2`).
Restored; **will not survive the next rebuild again**, belongs in the Dockerfile.

Done is the thread capacity as a
**promise** (`TARGET_THREADS = 10_000`, measured:
`4 Kern, 10000 Thread-Slots (5000 hostbar), Tabellen 7872 KiB aus dem RAM`), the
cap space telemetry (peak instead of end state — a run that touched the limit in between
and cleaned up afterwards looks harmless at the end) and, since part 2, the **cap space from the
boot RAM**.

**The find that justifies A-3.4 — closed:** about `CAP_BUDGET_PER_PD = 8` it was said
to prevent a cross-PD DoS. Recomputed that was `NPDS * CAP_BUDGET_PER_PD` = 256 × 8 =
2048 against **256** available slots: 32 PDs with a full budget filled the table, the 33rd got
nothing — exactly the DoS that the budget is meant to prevent. Part 2 turns `slots`/`objects` from
`[CapSlot; 256]`/`[Object; 128]` into `Slab<_>` and lets `configure_caps()` allocate them at boot,
dimensioned according to `CAP_SLOTS_FOR_ALL_PDS` (2048) + 256 reserve. Measured in the
x86 bring-up: `cap : 2304 Slots / 2304 Objekte, Tabellen 400 KiB aus dem RAM (Summe aller
PD-Budgets: 2048)` and `capsz : Hoechststand 11/2304 … bei vollem Budget passen 288 PDs in die
globale Tabelle`. **288 > 256** — the sum now fits in, without lowering the PD count.

Two side effects of part 2 that count independently: `audit_cdt` takes the counting
surface as a buffer from the caller and reports with code 8 "could not run" instead of silently "consistent";
`ipc_audit()` in `system.rs` bypassed the wrapper `cap_audit_cdt()` as the **only** site
and thereby the lock order (CAP_AUDIT before CAPS) — fixed. `MAX_FINALIZED` is gone,
`#![forbid(unsafe_code)]` now enforces in the cap crate what had previously only been claimed.

**Part 3 (`f6e5186`) connects the PD table:** `[Pd; NPDS]` in `.bss` was the reason why
10000 threads were not 10000 tenants — 256 address spaces, afterwards only shared ones. `pds` is
now a `Slab`, `configure_caps()` attaches it at boot, `NPDS` stands at **10000**. Measured:
`cap : 80256 Slots / 80256 Objekte / 10000 PDs, Tabellen 17792 KiB aus dem RAM`, plus
`PASS: A-3.4: 10000 PD-Slots`. The number costs RAM, no structure.

**The find in part 3 — no test artefact:** `create_vspace_masked` allocated the **third**
table level via the uncoloured `mem_alloc`. On x86_64 (PML4 → PDPT → PD) thereby **one of the
three** tables of every coloured PD lay outside its colour set — and the colour test did not see it,
because it reads back only `l1`/`l2`. A page walk of the MMU in the name of this PD leaves the same traces there
as in the other two. Pulled onto `mem_alloc_masked`; if the coloured
allocation fails, creating the VSpace fails instead of taking foreign colours along. **Touches
strand B** (stripe bookkeeping) — hence named here.

In addition, `colors.rs` now separates what are two findings: `kernelseite=` now means only "colour
holds", next to it stands `rueckgelesen=`. Before, a broken colouring (isolation error) and
a `0` from the return channel (error of the test) fell into the same bit — a `kernelseite=0` could not
be interpreted without guessing. Both still fail (`ok` demands both). The same separation
as for `audit_cdt` (code 8) since part 2.

**Part 4 (`25d388a`) closes the last fixed table of the chain:** `NENDPOINTS`/`NNOTIFICATIONS
= 32` in `caprock-ipc` meant that with `NPDS = 10000` every PD could indeed have its own address space,
but from the 33rd on none could be a server any more. Both are now `Slab<_>`, dimensioned at boot
according to "one PD, one endpoint" (`NPDS` + reserve = 10064) and attached in **both**
boot paths (`main.rs`, `bringup.rs`) before the self-test — the same `attach` mechanics
as CapSpace (part 2) and PD table (part 3). Measured: `ipc : 10064 Endpoints / 10064
Notifications, Tabellen 16908 KiB aus dem RAM (eine PD, ein Endpoint: 10000 PDs)`, plus
`PASS: A-3.4: 10064 Endpoints / 10064 Notifications`.

**The conclusion (`d1f5ed8`) — the sum is now checked, not just presupposed:**
`budget_allows` caps the consumption of **one** PD; that the sum of all budgets fits into the table
was so far borne solely by the dimensioning (10000 × 8 = 80000 + 256 reserve = exactly the
80256 slots). Unchecked remained the other side of the same calculation: **the kernel itself** installs
root caps, and nothing prevented it from occupying more than its 256 reserve. Then every
single PD is within its budget and nevertheless gets `NoSlot` — at a place that did nothing
wrong. `Caps::nonbudget_slots` counts the slots that go to no PD budget, expressly **not** via
`used_slots − Σ cap_count(pd)`: if two PDs hold the same `CapPtr`,
the sum overcounts, and the shortfall points in the unsafe direction — the check would pass
although it should not. Instead every held slot is marked and what remained occupied and unmarked is counted out;
a counting surface that is too small is a finding of its own (`None`), not a
silent "in order" (the same separation as code 8 in the CDT audit). In addition
`CAP_SLOTS_TOTAL`/`CAP_SLOTS_KERNEL_RESERVE` in `caprock-microkit` — the kernel had
copied `CAP_SLOTS_FOR_ALL_PDS + 256`, a second site that on the next turn of `NPDS` would silently
drift apart — and a check after `attach` that the table is really as large as
computed. Measured: `capsum : 1/256 Slots ausserhalb aller PD-Budgets (Kernel-Wurzelcaps);
Kapazitaet 80256 = 80000 PD-Budgets + 256 Reserve`.

**Next step:** A-3.4 is thereby **complete** — the dynamic tables stand (threads,
caps, objects, PDs, endpoints, notifications), and the sum is checked instead of presupposed. Next
A-4 (hot reload, 4.1–4.3), afterwards A-5.

**Proved by run 09:22** (`build/diag/a34-summe.log`): `rc_arm=0` (aarch64 build as
regression protection for the arch-neutral crates), `rc_load=0` with `== ALL PASS ==`, `rc_main=1` with
exactly one FAIL — `x2APIC`, the known TCG caveat without KVM. Part 4 previously proved by run
21:31 (`build/diag/a34-teil4.log`), same balance.

**Newly done (2026-07-30):** A-2.2 (`default = []`), A-4.4 (version lock in the loader, both
outcomes proved), A-4.5 (negative list `invariants.md` §13), A-3.4 part 1 to part 4 (see above).

**A limit that belongs to A-4.4 and is not concealed:** via the manifest the
rejection branch is today **not reachable** — per boot there is exactly one manifest. It becomes so only
with an exchange that brings a different image along at run time (A-4.1/A-4.3). So that it does not remain
unchecked until then, the self-test feeds the bookkeeping directly (`iface : ALL PASS`).

**Done and proved:**

| Item | Result | Commit |
|---|---|---|
| A-1.1 Multiboot modules | `mbmod : ALL PASS` — module areas are **cut out** of the free list (edge, overlap, unsorted, full coverage fed in) | `ee8029c` |
| A-1.2 Manifest format | 80-B header / 96-B entries, `entry_len` in the header; host-tested incl. mutation run, Kani proofs | `6d68328` |
| A-1.3 Manifest as authority document | signed, **bound to the kernel image** (SHA-256 over `[__text_start, __rodata_end)`); the check order is carried by the type (`Verified`) | `6d68328` |
| A-1.4 Policy fields | format stands and is reported; **they are not yet applied** — B takes over there | `6d68328` |
| A-1.5 `SYS_LOAD` on x86 | `load_by_index` is no longer a `None` stub; ELF parser knew only `EM_AARCH64` | `6d68328` |
| A-2.1 Root task | `root : ALL PASS` — loads further via its **own** loader cap (second badge proves it) | `6d68328` |
| A-3.1 `SYS_CDELETE` | syscall 14, checked from ring 3, **both** outcomes | `6d68328` |
| A-3.2 `SYS_CMOVE`/`CCOPY`/`SETRECV` | syscalls 15/16/17, rights intersection, badge, receive slot at the **receiver** | `6d68328` |
| A-2.2 Preparatory work | `--no-default-features` was **not compilable on aarch64**; test calls gated, **root task now also starts on ARM**. All four configurations build | `c413012` |

**Open and named** (not forgotten, but deliberately later):

* `CAP_PD_CONTROL` cannot be granted — the kernel **rejects a manifest** that demands it,
  instead of silently giving less. The honest way would be an ABI extension (`SYS_LOAD` returns the
  PdControl cap of the new PD); belongs to A-3.2, still pending.

**Blocked:** nothing. **A-2.2 is done** (2026-07-30) — B delivered the proof
(message 6), the turn `default = []` is committed. The suites demand `--features selftest`
for the booted build expressly.

**Next coupling, before A-3.4 starts:** B-4.2 now keeps books on the colour stripes, and
there are exactly four. A dynamic PD count against a static stripe count does not add up; the
proposal stands in [AGENTS.md](AGENTS.md) message 9 (way 2: `POLICY_EXCLUSIVE_STRIPE` from
A-1.4 — only whoever requests a stripe gets one).

**Test situation:** x86 suite unchanged as reported by B. aarch64 I cannot run
(B-2.1, `keys/` gitignored) — the four build configurations are **built, not run** there,
and the new ARM root task path is thereby **unchecked**. It is the same call as on x86, where it is
green; that is an argument, not proof.

---

## Linux compat (opencode)

**Last changed: 2026-09-11**

**State:** S0 **46/46** + `lx_messen` green. Syscalls wired: `SYS_PARK_TIMEOUT`(29),
`SYS_CALL_TIMEOUT`(30), `SYS_FORK`(31), `SYS_EXEC`(32), `SYS_DEBUG` v2 (33–35),
`SYS_LOAD_IMAGE`(36), `SYS_CSUB`(37). Timeout/preempt/kmalloc/page templates proved,
`lx-shim-demo` 9/9.

**Services:** mem-server stage 2 — protocol 21/21 + CSUB grants 28/28. RSP stage 2
(m/M/g/`qSupported`/?). WASM 2a: spec appendix + ENG + MEM + WASI, 27/27. EIME conditionally in.

**Drive 5:** verify-at-boot stands (`6d780cc`, span fallback, shortage details):
`[7]lxpd-minelf gestartet (PD=9)`, `vollzahl 7/7 ALL PASS`, `root ALL PASS`,
`SELFTEST COMPLETE`. E2E IRQ drive is running.

**Open:** B4b (`weckrufe=0`; address formation proved correct, mask false alarm refuted).
DEBUG 34/35 need `hal::debug` (HAL ownership, only patch text delivered).

**Audit findings (2026-09-10):** FNV turned back to the standard `...25c5` (`d23003e`,
vector tests + interop anchor); foreign `...2325` sites named (`caprock-lxpd`,
`lx_driver_manifest.py:40`, `lxpd_glue.rs:231`). #DF rule: no array >~256 B on
hot paths (`dispatch_fork` → heap + `#[inline(never)]`). Lazy guards (root fix).

**Next:** WASM engine 2a.

**ARM trio + pdfreeze (2026-09-11):** double allocation in the PD cspace pool found and
fixed (committed `765d662`, see AGENTS.md 19): PD 0/1 shared run `[32..48)` — stale
free-list link on slot reuse. Proved: cspace host 13/13,
aarch64 suite `ALL PASS`, x86 suite (512M) `ALL PASS`.

**Rewrite preliminary (2026-09-11, AGENTS.md 20):** builds rc=0, cspace 13/13. QEMU red is
load-correlated, as reasoned (no fix effect): ARM guest green, script red only via
`dbg` worst case (259k/388k green vs. 700k red, IDENTICAL binary, load 5–6);
x86 `sperre` 1.6 billion cycles (host deschedule) → watchdog → verif/sweep consequence.
Recommendation: `tools/abnahme.sh` on an idle machine.
