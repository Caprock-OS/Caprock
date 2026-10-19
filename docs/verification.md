# Caprock — Formal verification (tier 1: Kani, tier 2: Verus pilot)

This document describes Caprock's **permanent verification pipeline**. Strategy analysis
(tier model 1–3, seL4 comparison, tool landscape): `ARMTest/formale-verifikation-aufwand.md`.

## Tier model (short version)

- **Tier 0 (present):** safe-Rust core (memory safe by compiler), `forbid(unsafe_code)` parsers,
  10 run-time audits, fuzzers, `docs/invariants.md`.
- **Tier 1 (HERE, active):** **Kani** (bounded model checking, CBMC) proves freedom from panic/OOB/overflow
  + structure invariants of the clearly delimited, security-critical components.
- **Tier 2 (pilot active):** **Verus** — deductive functional correctness (audits become the spec).
  First small pilot implemented (see below).
- **Tier 3 (research):** full correctness + isolation/information flow + HW model.

## Tier 1 — what is proved

| Component | Harness | Statement |
|---|---|---|
| `caprock-loader/cert.rs` | `parse_never_panics` | `TrustedCert::parse` **never** panics/goes OOB/overflows (arbitrary input) |
| | `parse_partitions_input` | on success: `message()+signature()` partition the input exactly, sig not empty, `len==152+build_info` |
| `caprock-loader/archive.rs` | `parse_never_panics` | `Archive::parse` (+ `program(i)`) panic/OOB-free |
| `caprock-loader/elf.rs` | `parse_never_panics` | `ElfImage::parse` panic/OOB/overflow-free |
| | `segments_are_sound` | on success: every segment `memsz>=filesz`, `segment_bytes().len()==filesz`, `offset+filesz<=len` |
| `caprock-region/lib.rs` | `split_at_partitions` | `split_at` partitions exactly + gaplessly + non-overlappingly, overflow/underflow-free |
| | `subview_within_parent` | `subview` lies entirely within the parent region (`off+sublen<=len`, no escape) |
| | `get_set_never_oob` | raw `get`/`set` (over a real buffer) never access outside the region |
| | `copy_fill_never_oob` | raw `copy_from`/`copy_to`/`fill` respect bounds (no OOB) |
| `caprock-sync/lib.rs` | `spinlock_roundtrip` | `SpinLock`: guard deref memory-safe, lock/unlock round trip (`next==serving`), data persistence |
| | `rwlock_write_then_read` | `RwSpinLock`: write→read sees the value; after all drops state `0` |
| | `rwlock_state_arithmetic` | reader count + writer bit over/underflow-free; writer drop clears **only** the WRITER bit |

> **Reach for `caprock-sync`:** Kani is a **single-threaded** model checker, **not** a
> concurrency checker. Proved are memory safety of the guards, single-thread round trips +
> data persistence and the counter **arithmetic**. **Mutual exclusion under simultaneous
> multi-core access** (interleavings) lies **outside** Kani's reach — a
> concurrency model checker would be needed for that. Loom stage 2 has existed since `c2116ac` — **but does not cover
> the IRQ masking**, and precisely there lay the error of 2026-07-29 (see `invariants.md` §1a):
> on x86 `irq_save_disable` was a no-op, because the host branch also swept in the kernel target. Loom
> models a **copy** of the algorithm; an error in the `cfg` **selection** is invisible to every
> model, because the model does not even see the selected code. What was effective instead
> was an assurance about the selection itself (`IRQ_MASKING_IMPLEMENTED`, compile time).

The harnesses are `#[cfg(kani)]` modules directly in the respective source files — **completely inert in
the normal build** (no effect on kernel/tests). The input upper bounds (e.g. 200–260 B)
are chosen **path-complete**: every code path that continues demands structurally a
smaller input; larger fields always trigger the early reject. The proof is thus
effectively complete for these parsers, not merely "tested up to N".

## Running locally

```sh
# Einmalig:
cargo install --locked kani-verifier && cargo kani setup
# Alle Loader-Beweise:
tools/kani-verify.sh
# Ein einzelner Harness:
tools/kani-verify.sh --harness cert::kani_proofs::parse_never_panics
```

`tools/kani-verify.sh` copies the (dependency-free) crate to `$TMPDIR` and calls `cargo kani`
there — this circumvents the `build-std` requirement from the workspace `.cargo/config.toml` (custom target
for the bare-metal kernel), which would otherwise break `cargo kani`.

## CI gate

Two workflows run on **every push/PR**:
- `.gitea/workflows/kani.yml`: host tests of the loader crate + inert check + **all Kani harnesses**.
- `.gitea/workflows/verus.yml`: installs Verus (pinned release + Z3) + **all Verus proofs**
  (`tools/verus-verify.sh`).

If a proof fails (a later change violates a proved property), **the CI
fails**. Thus properties already proved — Kani **and** Verus — cannot be lost unintentionally;
verification is a fixed part of the development process.

## Tier 2 (pilot) — Verus: documented invariant as a formal specification

The difference from Kani: tier 1 proves **absence of errors** (panic/OOB/overflow) in a bounded way;
tier 2 proves **functional correctness** deductively — that an operation **preserves an invariant**,
for **all** states (unbounded). Method (prescribed by the user): **no** complex area
(scheduler/IPC), but an **already documented kernel invariant** as the first formal spec.

Several pilot files specify **documented kernel audits** and prove that the respective
operations **preserve** the invariant — statically + for **all** states, not only at the
audit quiescence points as at run time (`tools/verus-verify.sh`, in total **32 verified, 0 errors**
over 9 files, over **six** of the documented audits — `cap_audit_cdt`, `domain_audit`,
`vspace_audit`, `dma_audit`, `loader_audit`, `trust_audit`; six different invariant types:
counting, linked structure, classification, permission, geometry, hash consistency):

**(A) Refcount invariant** ([`verus/cap_cdt_refcount.rs`](../verus/cap_cdt_refcount.rs), codes 1–3):
1. every occupied slot points to a valid, occupied object;
2. `refcount(o) == ` number of occupied slots pointing to `o`;
3. object occupied **⟺** `refcount(o) > 0`.
Proved for **`install`** (new object, `refcount=1`), **`copy`** (slot + `refcount++`) and
**`delete`** (delete slot + `refcount--`, free the object at 0). Core: four lemmas proved by induction
about the `refs_to` counting function (push/update/member/fresh).

**(B) CDT sibling consistency** ([`verus/cap_cdt_tree.rs`](../verus/cap_cdt_tree.rs), code 5): the
sibling list of the derivation tree is a **doubly linked list** whose `next`/`prev`
are **mutual inverses** (and point only to valid, occupied nodes). Proved for
**`insert_before`** (insert at the list head) and **`unlink`** (remove a node, re-hang neighbours).

**(C) CDT structure invariant (unified)** ([`verus/cap_cdt_structure.rs`](../verus/cap_cdt_structure.rs),
codes 4-local + 5 + 6): a node model with object + all four linkings (parent/first_child/
next/prev). Invariant: derivation **shares the object** (`object[parent]==object[s]`, 4-local),
sibling inverse (5), `first_child` points back + is list head (`prev==None`, 6). Proved for
**`derive`** (deriving a capability = new child at the head of the child list). Core idea: the former
head has `prev==None`, so it is referenced by no `next` → inserting before it breaks nothing.

**(D) Domain policy** ([`verus/domain_policy.rs`](../verus/domain_policy.rs), `domain_audit` codes
1+2): hardware caps (MMIO/IRQ/DMA) may be held **only** by HardwareLand PDs, authority caps
(PdControl/Loader) **only** by TrustedSas. Proved that the gate **`install_cap_checked`** (installs
only policy-conformantly) preserves the classification invariant — an HW cap never lands in a
non-HardwareLand PD.

**(E) W^X** ([`verus/wx_invariant.rs`](../verus/wx_invariant.rs), `vspace_audit`): **no** mapped
EL0 page is both writable **and** executable (code integrity). Proved for **`map_page`**
(maps only W^X-conformantly) and **`make_writable`** (W only on non-executable pages).

**(F) DMA disjointness** ([`verus/dma_disjoint.rs`](../verus/dma_disjoint.rs), `dma_audit` inv. 1):
kernel-carved DMA regions are **pairwise disjoint** + disjoint from the kernel region
(basis of DMA isolation). Proved for **`alloc_region`** (inserts only disjoint regions).

**(G) CDT acyclicity** ([`verus/cap_cdt_acyclic.rs`](../verus/cap_cdt_acyclic.rs), `cap_audit_cdt`
code 7 — the **most difficult**): the parent chain has **no cycle of any length**. Proved
via a **well-foundedness measure** (a `rank` that falls strictly along `parent` → every chain
terminates). `derive` preserves the rank monotonicity; the lemma `ancestor_rank_decreases` (induction over
the chain length) delivers the **general** statement `not_own_ancestor`: no node is its own
`k`-th ancestor, for **arbitrary** `k`.

**(H) Loader use-after-free protection** ([`verus/loader_disjoint.rs`](../verus/loader_disjoint.rs),
`loader_audit`): no loaded segment overlaps a **free** RAM region. Proved for **`free_ram`**
(returns RAM only if disjoint from all segments).

**(I) TrustedSAS key DB consistency** ([`verus/trust_keydb.rs`](../verus/trust_keydb.rs), `trust_audit`):
the read-only key DB is **self-certifying** (`key_id == fingerprint(pubkey)`) + the `key_id`s
are **unique**. Proved for **`add_key`** (`key_id := fingerprint(pubkey)`, only if the key_id is free).
`fingerprint` is uninterpreted — the proof depends only on its functional property.

Thus `cap_audit_cdt`, `domain_audit`, `vspace_audit`, `dma_audit`, `loader_audit` + `trust_audit`
go from **checked** at run time to **proved** invariants — six of the documented audits.

Run locally: `tools/verus-verify.sh` (Verus + Z3 from the release into `~/.verus`; the required
rustc toolchain via `rustup toolchain install`). Strategy/tier model: `ARMTest/formale-verifikation-aufwand.md`.

**Next Verus steps (open, more difficult):** `delete_leaf` on the unified structure (more
case distinction than `derive`); the child-list **reachability** from code 4 (list reachability);
the **acyclicity** of the parent chain (code 7, needs a well-foundedness measure); then the remaining
audits as separate models (`domain_audit`, W^X/`vspace_audit`, `dma_audit`); afterwards step by step
towards scheduler/IPC.

> **Realistic classification:** This is a **permanently growing** verification of the documented
> kernel invariants, not a one-off goal. Full functional correctness + isolation/information flow + a
> hardware model + the **real SMP concurrency** (**not** captured by single-threaded Verus)
> are research class (tier 3, multi-year — see `ARMTest/formale-verifikation-aufwand.md`). Every
> invariant proved here is a closed, CI-capable building block on this path.

## Adding new components

1. Append `#[cfg(kani)] mod kani_proofs { use super::*; … }` to the source file.
2. Write harnesses with `#[kani::proof]` (+ `#[kani::unwind(N)]` for loops); inputs via
   `kani::any()` + `kani::assume(len <= MAXLEN)`.
3. Choose a **path-complete** input bound + justify it in a comment.
4. Locally `tools/kani-verify.sh` green → commit (the CI gate then checks it permanently).

## Roadmap tier 1

- [x] Cert parser (`cert.rs`)
- [x] Boot archive parser (`archive.rs`)
- [x] ELF parser (`elf.rs`)
- [x] CI gate (Gitea Actions)
- [x] Region runtime (`caprock-region`: RegionView — bounds/slice/overflow contracts around the `unsafe` blocks)
- [x] Synchronisation primitives (`caprock-sync`: memory safety/round trip/arithmetic, single-thread)
- [x] Concurrency model checking of the locks (Loom, stage 2, since `c2116ac`) — **with a limit
      that became expensive on 2026-07-29, see below**
- [ ] kernel-wide overflow/arithmetic checks
