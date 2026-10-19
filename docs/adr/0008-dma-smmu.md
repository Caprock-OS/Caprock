# ADR 0008 — DMA capabilities with SMMUv3 enforcement behind a generic DmaEnforcer abstraction

Status: **Accepted** (ext-23, implemented D0–D5)

## Context

With ext-22 Caprock has three kernel-separated domains and hardware caps for **MMIO** and
**IRQ**. **DMA** was deliberately deferred — the only HW cap category where a
bus-mastering device reads/writes physical memory **directly**, **bypassing the CPU MMU**. A
HardwareLand backend that programs a device's DMA registers could otherwise let the device
overwrite kernel or foreign-domain memory although the backend's *own* VSpace
is isolated — a direct breach of isolation invariant #1.

The long-term goal is real platforms (STM32MP25 series among others) with full hardware-backed
DMA isolation via ARM SMMUv3.

## Decision

**The DMA mechanism (`DmaCap`) is decoupled from the enforcement driver (`DmaEnforcer`); SMMUv3
is the implementation, not the architecture.**

### DMA mechanism — `DmaCap` (SMMU-agnostic)
`ObjectKind::Dma { phys, len }` is the authority over a **kernel-carved,
contiguous RAM region** serving as a DMA buffer. Minted kernel-side only (no
user syscall requests arbitrary physical ranges), installable only in **HardwareLand**
(`kind_is_hardware` includes `Dma` → `Caps::domain_audit` code 1). The capability model,
`install_dma_cap`, `alloc_dma_region`, the mapping (EL0-RW **normal non-cacheable** into the
isolated backend VSpace) and the backend interfaces are designed **without any SMMU reference**.

**Unlike Mmio/Irq, DMA is real RAM.** Finalisation (`delete_leaf`) returns the region via
`free_region` — DMA-use-after-free-safe **because** the revoke order guarantees
that no device can write into the region any more beforehand:

```
enforcer.disable_dma(binding)   // Durchsetzung entziehen (SMMU: STE invalidieren + TLBI + SYNC)
        ↓
VSpace-Unmap (zurück auf EL1-only) + flush_asid
        ↓
free_region(...)                // erst jetzt RAM freigeben
```

### Enforcement — `DmaEnforcer` trait (IOMMU-neutral)
A kernel-internal trait abstraction: `init` / `enable_dma(binding)` / `disable_dma(binding)` /
`audit`. A `DmaBinding` carries only generic IDs (StreamID, PhysRegion, backend PD), **no**
SMMU register details. The only place that knows SMMU registers/STE/CD/queues is
`SmmuV3Enforcer` (+ `hal::smmu`). The public DMA path speaks only the trait.
A future `NullIommuEnforcer` or other IOMMU driver implements the same trait **without**
touching public code (DmaCap, install_dma_cap, mapping, drivers, HardwareLand, audits).

### SMMUv3 implementation (stage 1, default abort)
`SmmuV3Enforcer` brings the SMMU up (command/event queue + linear stream table, CR0
SMMUEN|CMDQEN|EVENTQEN). The stream table is zeroed → **default abort** (every stream
aborts until an STE is installed). `enable_dma` installs, per bound device, exactly one
**STE → CD → stage-1 page table** (VMSAv8-64, T0SZ=25, 4 KiB granule) that **exclusively**
identity-maps the DmaCap region; everything else stays unmapped → translation fault (event queue).

### Address programming backend-direct, kernel-audited
virtio puts buffer physical addresses into RAM **descriptors** (not into MMIO registers), so
no kernel-mediated register check applies. Instead the trusted backend builds rings with
physical addresses from its DmaCap; the SMMU is the hardware-side enforcing instance.

## Two-level enforcement (user decision)

During validation it turned out (via QEMU SMMU tracing, see below) that for **emulated**
devices QEMU does not route DMA through the SMMU translate path — hardware-side enforcement
cannot be *demonstrated* under QEMU with an emulated device. The architecture was thereupon
laid out in two levels:

- **Level 1 — software discipline (hardware-independent, demonstrable):** the trusted
  driver validates via `dma_addr_in_region` **every** device DMA descriptor address against the
  DmaCap **before** it programs the device. An out-of-window address is rejected → the
  device is not even programmed. Works on every platform; proved in the QEMU test run.
- **Level 2 — SMMUv3 (hardware backstop):** the installed stage-1 STE restricts the
  device's actual bus access on the hardware side — the backstop for the case that a
  compromised/faulty backend bypasses the software check. Takes effect on real hardware.

## Alternatives compared

- **No IOMMU, software only:** tractable, but no HW guarantee. **Rejected** (security #1).
- **SMMU mandatory (original stipulation):** highest guarantee, but not portable to SoCs
  without an IOMMU and not demonstrable in QEMU with emulated devices. **Replaced** by the
  two-level model (level 1 demonstrable + portable, level 2 as HW backstop).
- **Stage 2 / nested SMMU:** more powerful (guest virtualisation), unnecessary here. **Rejected.**

## Security invariants (secured by oracles)

`dma_audit()` (aggregated in `ipc_audit` as code 40+):
1. DmaCap regions page-aligned, in the mappable GiB-1 window, disjoint from the kernel image
   (`floor = kernel_end`) — a region above the kernel image is rejected.
2. Two different DmaCap regions never overlap (object-granular, cap copies count once).
3. Revoke order (`disable_dma` → unmap → free) → no DMA use-after-free.
4. `enforcer.audit()`: no global SMMU errors (GERROR), event queue empty in normal operation.

Resource baseline (hwfuzz): MMIO/IRQ delete does **not** touch the RAM allocator (device != RAM);
**DMA** alloc ↔ free is **balanced** (region back to baseline per epoch).

## Finding: QEMU SMMUv3 + emulated devices

Shown via QEMU SMMU tracing (`-trace 'smmu*'`, 8 runs): QEMU 11 does create the IOMMU memory
regions (`smmu_add_mr`) and processes all SMMU commands (CR0, CMD_SYNC, CFGI_STE, TLBI), but does
**not** route the DMA of **emulated** devices (here virtio-rng-pci) through `smmuv3_translate` — **zero**
`smmuv3_translate*`/`smmu_ptw*`/`decode_cd` events, both as an integrated bus-0 endpoint and
behind a `pcie-root-port` (bus 1, RID 0x100), and even with a **V=0** STE (which according to the spec
would have to abort) the DMA succeeds. Cause: QEMU's emulated SMMUv3 does not translate the bus access for purely emulated devices
(without IOMMU MAP/UNMAP notifiers, which only VFIO passthrough registers).

**Consequence:** the level-2 hardware enforcement is **not observable** under QEMU with emulated devices
(`cj_smmu_enforced=false`, reported honestly). The stage-1 configuration is nevertheless
installed correctly and would take effect on real hardware. The demonstrable enforcement in the
test run is provided by level 1 (software bounds); the crown-jewel test additionally shows that the
software check is load-bearing (without it the device writes the out-of-window target).

## Consequences

- DMA-capable drivers (e.g. NVMe, NIC) live as HardwareLand backends with DmaCap + MmioCap;
  the SMMU restricts bus access on real HW, the software discipline on every platform.
- Portable: the same architecture/API on SoCs **with** and without an SMMU; the enforcer is exchangeable.
- Deliberately deferred: stage 2/nested, several simultaneous DMA devices, hotplug, scatter-gather
  across several DmaCaps, a real `NullIommuEnforcer`, verification of the hardware enforcement on
  real SMMU hardware (or the QEMU VFIO path).
