# ADR 0017 — Formal functional verification of the region runtime (Verus, phase 3)

Status: **accepted** · Date: 2026-06-27 · Phase 3 of functional verification.
Reference: ADR 0010 (region runtime + process heap), ADR 0015 (verification approach), `Verification/region-runtime/`.

## Context

The region runtime (`caprock-region`) is the **only** crate with memory `unsafe` in the userland path;
its **memory safety** (RegionView accesses never OOB) has already been proved with **Kani**
(`docs/verification.md`). Phase 3 adds the **functional** layer: the allocator's **resource balance** —
conservation, ownership, balance —, which is checked at run time by `churn`/`sasheap`
(`total_free` baseline).

## Comparison of variants

| Variant | Description | Assessment |
|---|---|---|
| V1 full hybrid allocator (slabs+bump) | model the real allocator data structures | large; the slab/bump geometry is memory safety (Kani) + disjointness (cf. dma_disjoint), not the core **balance** |
| **V2 balance model (chosen)** | pool as `total`/`free` + region seq; **conservation** `free + Σ(live) == total`, alloc/free against it | captures exactly the functional property (no leak, no double free, balance); small + faithful |

## Decision

**V2** — model the **conservation invariant** `free + Σ(live regions) == total` and
prove that `alloc`/`free_region` **preserve** it, that `free_region` requires a **live** region
(no double free = linear ownership) and that `alloc(n)`+`free` restores `free` **exactly**
(**balance**, no leak). Abstract model; real code unchanged; the
memory safety of the RegionViews is delivered by Kani.

## Consequences

- **Positive:** the resource balance (run-time `total_free` checks) goes from "checked" to "proved";
  ownership/no-double-free formalised. Layering: Kani (memory safety) + Verus (balance).
- **Limits/open:** hybrid-allocator **geometry** (slab/bump disjointness), zero-copy invariants
  (one frame in two VSpaces, cap-granted), hot-reload state hand-over — later stages; the
  RegionSource grow/shrink is the next one (builds on the conservation).
