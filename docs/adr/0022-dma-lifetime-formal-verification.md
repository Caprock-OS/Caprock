# ADR 0022 — Formal verification of the DMA lifecycle (Verus, phase 7)

Status: **accepted** · Date: 2026-06-27 · Phase 7 of functional verification (further component).
Reference: ADR 0015 (verification approach), ADR 0008 (DMA/SMMU), `Verification/dma-lifetime/`,
run-time `dma_audit` (code 4), `docs/invariants.md` §2.

## Context

DMA is the only HW cap category where a bus-mastering device reads/writes physical memory **directly**
— **bypassing the CPU MMU**. If a DMA region is freed while a device may still DMA into it
(enforcement not withdrawn), the device writes into already recycled RAM (**DMA use-
after-free**) — a direct breach of isolation. The safe revoke order is
`detach → (unmap) → free` (`revoke_dma`); at run time `dma_audit` code 4 checks ("a region still mapped in an
SMMU context never overlaps free RAM"). Phase 7 **proves** the underlying
lifecycle invariant. The **SMMU hardware enforcement itself** + **concurrency** stay
**outside** (HAL/concurrency TCB).

## Comparison of variants

| Variant | Description | Assessment |
|---|---|---|
| V1 geometric disjointness model | address intervals + `overlaps_free` bit-exactly | disjointness is already separate (`verus/dma_disjoint.rs`); here it is about the **temporal** order, not the geometry |
| **V2 lifecycle state machine (chosen)** | region as `{attached, freed}` flags; `attach`/`detach`/`free` as transitions | captures **exactly** the use-after-free property (code 4) as an ordering invariant; small + faithful to the `revoke_dma` reality |

## Decision

**V2** — model and prove the DMA lifecycle as a state machine: **invariant**
`attached ⟹ ¬freed` (a DMA-enabled region is never freed — = `dma_audit` code 4). Transitions:
`attach` (only on a live region), `detach` (always, idempotent), `free` (requires a **detached**
region). Proved: all transitions preserve the invariant; **no DMA after free** (a freed region
is never attached/attachable); `free` enforces the **revoke order** (`detach` necessarily first); the
canonical `detach → free` sequence is **always** safe. Abstract model; real code unchanged.

## Consequences

- **Positive:** the DMA use-after-free safety (revoke order) is proved as an ordering invariant,
  complements `dma_audit` (code 4) + the hwfuzz. Together with `verus/dma_disjoint.rs` (geometry)
  the DMA safety core (bounds disjointness **and** lifecycle) is formally covered.
- **Limits/open:** the **SMMU hardware enforcement** (STE/CD/stage 1 really enforces the isolation)
  is the HAL TCB; **concurrency** (simultaneous attach/free across cores) + the **aggregation
  of several regions per context** remain expressly outside (follow-up stage or HW/concurrency TCB).
