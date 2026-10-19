# ADR 0009 — Generic DMA infrastructure (direction, coherence, multi-region, scatter-gather, stream groups)

Status: **Accepted** (ext-24, implemented E0–E5)

## Context

ext-23 delivered `DmaCap` (one region) + the `DmaEnforcer` abstraction (SMMUv3) + level-1
software bounds. Before concrete DMA devices (virtio-net, NVMe, USB, Ethernet) are implemented,
the device-independent DMA basics are to be provided once, generically,
so that every future HardwareLand backend builds on them without an architecture change.

**Hard requirement:** break no existing API — purely **additive** (new enum fields with
defaults, new trait methods with a default impl, new `_ex` constructors next to the old ones).
ext-23 + the existing tests remain valid.

## Decision

### 1. Direction + cache coherence as **DmaCap attributes** (cap-pure)
`ObjectKind::Dma { phys, len, dir, coherence }` with `DmaDir { DeviceRead | DeviceWrite |
Bidirectional }` and `DmaCoherence { Coherent | NonCoherent }`. The cap encodes the full
authority including direction/coherence. `install_dma`/`install_dma_cap` remain (defaults
Bidirectional/NonCoherent = ext-23 behaviour); `install_dma_ex`/`install_dma_cap_ex` new.

- **Direction-minimal hardware rights (security gain):** the enforcer maps the stage-1
  side according to direction — `DeviceRead` → **read-only** (the device cannot corrupt the
  buffer), `DeviceWrite`/`Bidirectional` → RW. (Rejected: direction only at binding —
  the authority would not be fully in the cap.)
- **Coherence** → memory attributes: `Coherent` → Normal-WB (cacheable) + cache maintenance,
  `NonCoherent` → Normal-NC (default). Backend VSpace mapping (`vspace_map_dma`) is
  coherence-aware.

### 2. **DmaHandle** abstraction (IOVA ≠ PA decoupled)
`DmaHandle { iova, len }` is the *device-visible* address. Backends program the device
with `handle.iova`. Today `iova == phys` (identity), but all paths go through the handle —
so that later, **without an API break**: scatter-gather compaction into a contiguous IOVA
window, bounce buffers for 32-bit devices, non-identity remaps. (Rejected: IOVA=PA
hard-coded — a later decoupling would have to touch the DMA API.)

### 3. **DmaContext** (several regions per StreamID; stream groups)
One translation context per StreamID **group**: 1..N STEs → **one** CD → **one** stage-1
table that maps **several** regions. `dma_attach(stream_id, dma_cap)` attaches a region
additively to the (possibly new) context and returns a `DmaHandle`; `dma_detach` removes
it (last region → context teardown). `dma_group_add(leader, member)` lets several StreamIDs
share the same context (multi-function/SR-IOV/bridge without RID translation). Replaces the
ext-23 1:1 binding; `enable_dma`/`disable_dma` are mapped onto it backward-compatibly
(1 region = context with 1 region). (Rejected: `DmaCap` holds a region list — a heavy cap
object, would break the "1 cap = 1 region" model.)

### 4. **Scatter-gather** = framework-validated list over the attached regions
`DmaSgEntry { handle, offset, len }`; `dma_sg_validate(stream_id, &[…])` checks (level 1) every
segment against the region set of the context. The **device-specific** descriptor format (virtio
desc, NVMe PRP/SGL, NIC ring) is built by the backend from validated segments — the SG logic stays
device-independent. (Rejected: SG logic per device in the kernel — device knowledge in the kernel.)

### 5. Improvements beyond the initial list
- **Cache maintenance primitives** `dma_prepare(handle,dir)`/`dma_complete(handle,dir)`
  (internally `dc cvac`/`dc civac` + `dsb`) — backends call them around transfers, device-independently.
- **DmaPool** — bump sub-allocator over an attached region (descriptor rings, mbufs); every
  sub-buffer is a `DmaHandle` inside the parent region that is already SMMU-mapped + level-1-validatable.
  **(Removed in consolidation K2:** redundant — a sub-buffer is just a
  sub-range `[handle.iova+offset, +len)`, validated via the canonical SG/containment path
  `DmaSgEntry`+`dma_sg_validate`→`region_contains`. A backend keeps a trivial
  offset cursor itself if needed. See `docs/phase-reports/consolidation-ext22-25.md`.)**

## Layering (still load-bearing)

```
DmaCap (Mechanismus)         ── Region + Richtung + Kohärenz, Ownership/Bounds/Lifetime/Audit
   │ dma_attach / dma_detach / dma_group_add
DmaEnforcer (Treiber)        ── SmmuV3Enforcer: DmaContext (STE-Gruppe -> CD -> Stage-1, N Regionen)
   │ DmaHandle{iova}         ── iova=PA heute, entkoppelt für später
Backend (Service)            ── DmaPool, DmaSgEntry + dma_sg_validate, dma_prepare/complete
                                geräte­spezifischer Deskriptor bleibt hier
```

## Finding continuity (ext-23)

The SMMU hardware enforcement is still not observable under QEMU for emulated devices
(see [ADR 0008](0008-dma-smmu.md)). ext-24 is therefore verified **structurally** (stage-1 leaves
read back: direction AP + coherence attribute) + **functionally** on the level-1 software
layer (multi-region validation, SG validation, DmaPool, context/group bookkeeping). On
real SMMU hardware (STM32MP25 among others) the direction-minimal stage-1 configuration takes effect.

## Consequences

- Future DMA devices: `install_dma_cap_ex` (direction/coherence) → `dma_attach` (one or
  several regions, possibly a group) → DmaPool/SG as needed → `dma_prepare`/`dma_complete` around
  transfers. No architecture change necessary.
- Audits: `dma_audit` (bounds/disjointness, code 40+) unchanged; direction/coherence do not affect
  the bounds. hwfuzz additionally churns direction/coherence + attach/SG/detach.
- Deliberately deferred: non-identity IOVA allocation (bounce/remap) behind the `DmaHandle`,
  stage 2/nested, DMA pool as a slab (instead of bump), device-specific SG descriptor builders.
