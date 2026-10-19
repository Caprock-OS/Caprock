# ADR 0010 — Region runtime + process-local hybrid heap (safe-Rust API, encapsulated `unsafe`)

Status: **Accepted** (ext-25, implemented R0–R4)

## Context

ADR 0002 (SAS) demands: every address is real-physical, no MMU isolation, memory safety
exclusively via capabilities + safe Rust. Until ext-24, trusted-SAS processes had **no
dynamic RAM** (`#![no_std]`, no `#[global_allocator]`, no `Box`/`Vec`); cap-granted
regions were reachable only via raw `peek_u64`/`poke_u64`. Goal of ext-25: a **real
dynamic heap** (Box/Vec/BTreeMap) for safe-Rust SAS processes on real physical addresses — without
`unsafe` in app code, without a general `MemoryCap -> &mut [u8]` wrapper exposed outward.

## Decision

### 1. Runtime layer `caprock-region` — the ONLY place with memory `unsafe`
Two types encapsulate all raw access; the public API is entirely safe Rust:

- **`Region`** — the **cap-owned** unit: holds a `MemoryCap` (linear ownership) +
  audit metadata (`RegionTag { id, Purpose }`). `phys()`/`len()`/`into_cap()`/`view()`. A
  process holds a **set** of them — never "one heap".
- **`RegionView<'a>`** — a borrowed, **bounded** view. Offers only safe operations:
  - typed **copying** accesses `get<T: Pod>`/`set<T: Pod>`/`copy_from`/`copy_to`/`fill`,
  - **scoped** `with_bytes(|s: &mut [u8]| …)` — the slice is bound to the closure and
    **cannot leave** it (ergonomic for parsing/memcpy/DMA filling, without a bare slice
    leaking outward),
  - `split_at`/`subview` — the operations the allocator works on.

The few `unsafe` blocks (in the `RegionView` accessors + the allocator glue) are justified by the
**cap-validated** region (ownership + bounds + rights) + the bounds checks and are auditable.

(Rejected: a public `region_as_slice() -> &mut [u8]` wrapper — it would distribute bare slices
everywhere and not separate heap/DMA/hot-reload/audit metadata.)

### 2. Multi-region from the start + `RegionSource` (grow/shrink)
A process heap owns a **region list**, not one region. A `RegionSource` trait
abstracts grow/shrink:

```
trait RegionSource { fn request(min_len, Purpose) -> Option<Region>; fn release(Region); }
```

On the kernel/trusted-SAS side `KernelRegionSource` serves it directly from the physical allocator
(`MEM`); an EL0 process would marshal the same via syscall — the interface stays identical.

### 3. Hybrid allocator (chosen after comparison) — `Heap<S: RegionSource>` ⊳ `core::alloc::Allocator`
- **Small** (≤ largest class): **size-class slabs** (16…2048 B). Per class an **intrusive**
  free list of freed slots; new slots are cut from a **bump region**
  (bounded fragmentation, O(1)). If a bump region is full → a new one via `RegionSource` (grow).
- **Large** (> largest class): a **dedicated** region per allocation (contiguous, suitable for DMA/hot
  reload); on `dealloc` immediately back to the source (shrink). `Heap::drop` returns **all**
  regions.

Alternatives compared (for the SAS, **without** compaction):
- *Bump/arena only:* optimal in 5/6 criteria, but no mixed-lifetime heap → used only as a large/
  arena path.
- *Region free list (classic malloc):* permanent fragmentation in the SAS (no compaction)
  + heaviest verification load (coalescing invariants) → **rejected**.
- *Pure size classes:* good, but DMA/state not cleanly separated → extended to the hybrid.

Rationale: the hybrid meets fragmentation / performance / hot reload / zero-copy / DMA /
verification **at the same time** and keeps the `unsafe` minimal (bump trivial + slab simple, **no**
coalescing malloc).

### 4. No implicit global heap
The SAS model demands **process-local** heap instances (`Heap::new(source)` + `Box::new_in`/
`Vec::new_in`/`BTreeMap::new_in`). The `#[global_allocator]` is a **guard** (`NoGlobalHeap`)
that panics on accidental `Box::new`/`Vec::new` — an implicit, cross-process heap
deliberately does not exist (which regions of which process?).

## Trust/security model

With *no unsafe in app code + a bug-free compiler* (ADR 0002): a safe-Rust process cannot forge a
pointer and can only touch memory reachable from its legitimate references —
stack, static data, **its heap** (= its `Region` set). The `caprock-region` runtime is
the **small, clearly auditable** layer in which all memory `unsafe` is concentrated;
its correctness (bounds + cap ownership) carries the isolation, **without** an MMU. Capabilities govern
*which* regions the kernel grants at all (least privilege).

## Consequences

- Trusted-SAS processes can now use `Box`/`Vec`/`BTreeMap` on real physical addresses; the
  heap grows/shrinks via `RegionSource`.
- **Future without an API break:** DMA regions are the same `Region`/`RegionView` (Purpose::Dma; the
  ext-24 `DmaPool` is a special case); hot-reload state lives in a `Region` that is handed to
  v2 via cap (identity-stable address); audit/debug info hangs on `RegionTag`.
- **SAS prices (deliberate):** heap = non-contiguous regions; **no** live compaction
  (references must not be moved); fragmentation visible → countered with size classes.
- **Deferred:** slab region reclaim when a region is entirely free (requires free-list
  filtering), EL0 syscall `RegionSource` (kernel side directly), slab batching, `realloc` fastpath
  (currently via the `Allocator` default grow = alloc+copy+dealloc).
