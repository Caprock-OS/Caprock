# ADR 0003 — Capability system and physical memory model

**Status:** implemented (core) · **Date:** 2026-06-23

> **Implementation status:**
> - *Phase 2* (`crates/caprock-mem`): physical memory model — `MemoryCap`
>   as a linear (move-only) capability, `PhysAllocator` (coalescing free list).
> - *Phase 3* (`crates/caprock-cap`): the **CDT core** is implemented —
>   `CapSpace` (type-safe slots + derivation tree), object table with refcount
>   and finalisation, generation handles (`CapPtr`), operations `copy`/`mint`/
>   `move`/`delete`/`revoke` (rights can only be restricted). All safe Rust
>   (0 unsafe). See `docs/phase-reports/phase-3.md`.
> - **Still open** (later phases): further `ObjectKind`s (endpoint/notification/
>   TCB/device), several cspaces + multi-level CNode addressing (guards/radix),
>   dynamic backing store for caps/objects. The design sketched below remains
>   the guideline.

## Motivation

Capabilities are the only source of authority in the system: a process may do exactly
what it holds a valid capability for (memory, IPC, devices, services,
process management). In the SAS design (ADR 0002) they replace the missing
MMU isolation as the authority layer.

## Reference: the seL4 model

seL4 stores capabilities in *CTEs* (cap + MDB node), organised in *CNodes*
(addressed like a radix tree), and manages derivations via a *capability
derivation tree* (CDT/MDB). Core operations: `copy`, `mint` (restrict rights
+ badge), `move`, `delete`, `revoke` (recursively all descendants).
Memory arises from *untyped* regions via `retype`. See
[`../seL4-architecture-map.md`](../seL4-architecture-map.md).

## Approaches analysed (representation)

### A) True-to-seL4, pointer-based CTE/CNode structure in `unsafe` Rust
- **+** Well known, fastpath-capable.
- **−** Lots of `unsafe` (raw pointers, intrusive lists) — contradicts the
  unsafe minimisation. Rejected as the default.

### B) Type-safe CNode as slot tables + CDT as an arena with indices (chosen)
CNodes are `Slot` arrays; the derivation tree is an **arena** (`Vec`-like
pool in kernel-owned, capability-managed memory) with **index handles**
instead of raw pointers. Capabilities are a Rust `enum` with a type tag + rights mask
+ optional badge.

- **+** Memory safe without `unsafe`: index handles instead of pointers, the
  borrow checker protects the structures.
- **+** `enum` makes cap types explicit and exhaustively matchable.
- **+** Revocation = tree traversal over indices; can be bounded deterministically
  (the seL4 zombie technique for long revokes can be adopted).
- **−** Arena indices need generation counters against stale handles
  (well manageable, costs one word per slot).

### C) Pure capability-pointer machine without a CDT
- **−** Without a derivation tree there is no correct recursive revocation. Rejected.

## Decision

**Approach B.** Capabilities as a Rust `enum` with rights mask and badge; CNodes as
type-safe slot tables; CDT as a generation-protected arena with index handles.
Operations `copy/mint/move/delete/revoke` as safe methods; `unsafe` only
where cap-managed raw memory is initially typed (retype boundary).

**Capability classes (planned):** `Memory` (physical region + rights
RWX/owner), `Endpoint`, `Notification`, `Reply`, `Tcb`/`Process`, `Device`
(MMIO region/IRQ), `Service` (named service endpoint), `CNode`.

## Physical memory model (phase 2)

- At boot the root process receives **memory capabilities** over all
  free physical RAM (one or a few large regions, determined from the DTB).
- A **capability-based allocator** hands out sub-regions by *splitting*
  a memory cap (analogous to seL4 `retype`, but on physical regions instead of
  untyped objects). Splits are children in the CDT of the parent cap.
- Processes can **request** RAM (split from a region they own),
  **return** it (merge/delete of the child cap), **transfer** it (move/copy of the cap
  via IPC) and **receive it from parents** (delegation at spawn).
- Since address == physical address (ADR 0002), a memory cap directly describes a
  real range `[base, base+len)` with rights — no mapping needed.

## Security implications

Least privilege by construction; delegation and revocation via the CDT; no
ambient authority. The correctness of revocation is security-critical and is
secured with tests (phase 3) and, prospectively, with model checking.

## Performance implications

Cap lookup is an array index into CNode slots (O(depth), small). The fastpath
(ADR 0004) caches resolved endpoint caps to avoid lookups in the hot path.
Arena instead of allocations per cap keeps the operations alloc-free and
deterministic.
