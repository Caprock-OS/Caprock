# ADR 0007 — Three security domains (Trusted-SAS / HardwareLand / UserLand)

Status: **Accepted** (ext-22, implemented P1–P6)

## Context

Up to ext-21 Caprock had two *operational* process classes: SAS PDs (global
address space, trustworthy Rust) and isolated PDs (own VSpace+ASID). There was
**no** domain concept, no hardware capabilities (IRQs/MMIO were kernel-internal),
no management capabilities and no enforced communication policy — communication
was purely based on cap possession.

Goal: three **kernel-separated, auditable** domains, so that almost all OS code
remains memory-safe Rust and only the unavoidable hardware accesses take place in small,
isolated backends.

## Decision

**Domain = policy, VSpace = isolation mechanism.** Every protection domain carries an
**immutable** domain attribute that determines the permitted cap types and
communication relationships. The own VSpace merely enforces the isolation.

- **Trusted-SAS** — global address space (VSPACE_OF==0), only memory-safe Rust without
  `unsafe`. Driver/protocol logic. Default for all legacy PDs.
- **HardwareLand** — isolated VSpace, holds hardware caps (MMIO/IRQ), small `unsafe` core.
  Exactly **one** immutable Trusted-SAS partner; communicates exclusively with it.
- **UserLand** — isolated VSpace, no hardware rights whatsoever, no direct communication
  with HardwareLand.

### Variants compared (domain model)
- **A — pure cap convention (no kernel tag):** minimal, but no enforceable/
  auditable invariant. **Rejected.**
- **B — domain tag on the `Pd` + kernel enforcement (chosen):** auditable, enforceable,
  composes with the existing PD/VSpace/cap model without a mediator; enables a
  `domain_audit()` oracle.
- **C — domain as a cap object + policy server:** heavyweight, the mediator becomes a bottleneck.
  **Rejected.**

### Enforced invariants
1. **Cap-type policy** (`Caps::install_cap_checked`, central point): hardware caps
   (`Mmio`/`Irq`, generic category `kind_is_hardware` — `Dma` can be hooked in later without an ABI change)
   **only** in HardwareLand; `PdControl` **only** in TrustedSas.
2. **Isolation**: untrusted domains (HardwareLand/UserLand) **must** run isolated
   (VSPACE_OF!=0). TrustedSas may run isolated OR global (more isolation is not a
   violation — important for backward compatibility). Checked only for **live** threads.
3. **Immutable pairwise binding**: a HardwareLand backend is bound at creation
   (`create_hardware_backend`) to exactly one TrustedSas partner + exactly one channel
   (endpoint+notification); it may hold **only** communication caps for this channel.
   Cardinality **1 trusted : N HardwareLand** (structural; N:1 excluded,
   since `partner` is a single field). Stable `backend_id` (for future multi-device/hotplug).
4. **Management only cap-gated** (`ObjectKind::PdControl` + `SYS_PDCTL`): only a TrustedSas
   PD with the `PdControl` cap for a UserLand target PD may control its lifecycle
   (PAUSE/RESUME/START/STOP); every op is gated on cap + caller==TrustedSas + target==UserLand.
5. **W^X also for device mappings**: device pages are always PXN|UXN; `vspace_device_wx_ok`
   checks it.

All of this is bundled in the `domain_audit()` oracle (codes 1=HW cap wrong domain,
2=PdControl wrong domain, 3=untrusted runs global, 4=backend without trusted partner,
5=backend holds foreign comm cap) and aggregated in `ipc_audit()` (code 30+) — both fuzzers
check it continuously.

### Generic hardware infrastructure (HardwareLand)
- **MMIO**: `vspace_map_device` maps **arbitrary** MMIO regions EL0-device (Device-nGnRnE,
  PXN|UXN, nG) into an isolated VSpace; splits the GiB-0 1-GiB device block into
  L2/L3 if needed (rest EL1-only → kernel MMIO stays reachable). `MmioCap` can only be minted
  kernel-side (no user syscall requests arbitrary ranges), installable only in HardwareLand.
  **`Mmio`/`Irq` delete NEVER touches the RAM allocator** (device != RAM).
- **IRQ**: `IrqCap` + `route_spi` (GICD_ITARGETSR — was missing; without routing an SPI
  reaches no core) + **deferred delivery**: `handle_irq` (IRQ context) is LOCK-FREE (atomics +
  GIC mask + EOI), sets pending; the reschedule path drains BEFORE the SCHEDS lock and
  signals the bound notification (`signal_from_kernel`).

The PL031 RTC is the first proof of concept of this generic infrastructure (MMIO read +
match IRQ); UART/VirtIO/NVMe/USB/GPU use the same primitives unchanged.

## IRQ lock safety (make-or-break)

The deferred IRQ delivery is **deadlock-free** because:
- IRQs are **masked throughout the trap** (boot.rs `daifset #0xf`; DAIF is only restored on `eret`
  from SPSR). A core holding `NTFNS`/`SCHEDS`/`EPS` (always inside
  a trap) can therefore **not** take an IRQ → **no same-core reentrancy**.
- `handle_irq`/`irq_hook` take **no** lock (only atomics + GIC MMIO).
- `drain_pending_irqs` runs in the reschedule path (trap, IRQs masked) **before** the SCHEDS lock
  and takes `NTFNS < SCHEDS` — the established order. Cross-core there is only **bounded**
  spinning (no cycles, same order as all paths).

**Dependency (documented):** this guarantee rests on NTFNS/SCHEDS sections
running with masked IRQs (in-trap). Should a path ever release IRQs *inside* a trap,
`drain_pending_irqs` would still have to run masked.

## Consequences
- HardwareLand is fully proved with **MMIO + IRQ** (RTC on the real PL031).
- `DmaCap` stays **reserved** as an HW cap category (can be retrofitted without an ABI/structure
  change) — implementation only once a DMA device/virtio is available.
- Existing 36 checks remain green unchanged (default domain TrustedSas ⇒ new checks
  are no-ops for the legacy stock). `./test-qemu.sh` = **42/42 ALL PASS**.
- Sensitivity-checked: every new invariant broken individually → the responsible oracle/the
  test catches it (domain rule 3, pdctl policy, chan channel enforcement, rtc device mapping,
  irq SPI routing, hwfuzz baseline).
