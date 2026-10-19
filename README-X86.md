# Caprock — x86_64 port (branch `arch/x86_64`)

Port of the aarch64 microkernel to **x86_64**, staged with frequent QEMU runs. Goal:
**one branch per architecture** — `master` = aarch64 (QEMU `virt`), `arch/x86_64` = x86_64
(QEMU `pc`/`q35`). The code is separated via `cfg(target_arch = …)`, so that **both architectures
remain buildable on this branch** (regression safety): `./build.sh` builds aarch64, `./build-x86.sh`
builds x86_64.

## Building + testing

```sh
./build-x86.sh                 # -> build/target/x86_64-unknown-none/release/caprock-kernel(.mb32)
./test-qemu-x86.sh             # baut + bootet unter qemu-system-x86_64 + prueft Marker
./build.sh && ./test-qemu.sh   # aarch64 weiterhin unveraendert gruen (Regression)
```

## Boot path (x86_64)

Built-in Rust target **`x86_64-unknown-none`** (bare-metal, soft-float, no SSE, static). The
kernel is a **Multiboot1** image (header in `.multiboot`, linker `kernel/x86_64-link.ld`, loaded
@ 1 MiB). QEMU's Multiboot1 loader accepts only **ELF32**, but the Rust target produces ELF64 →
`build-x86.sh` casts the ELF **container** to ELF32 via `objcopy -O elf32-i386` (`*.mb32`); all
load/entry addresses are < 4 GiB, the code remains 64-bit. QEMU enters at the 32-bit `_start`:

```
_start (.code32): PML4/PDPT nullen -> 1 GiB Identity-Map (2-MiB-Seiten) -> CR3 ->
                  CR4.PAE -> EFER.LME -> CR0.PG -> lgdt(64-bit) -> retf CS=0x08 ->
long_mode (.code64): Datensegmente, Stack, .bss nullen -> x86_rust_entry()
```
(`retf` far return instead of `ljmp` — more robust in LLVM Intel syntax. `.boot.bss` with the boot page
tables lies in the linker OUTSIDE `[__bss_start,__bss_end)`, so that the `.bss` zeroing in long
mode does not wipe the active identity map; PML4/PDPT are zeroed in the 32-bit trampoline.)

## State

| Stage | Contents | Status |
|---|---|---|
| **0** | boot (Multiboot→long mode) + 16550 serial (COM1) | ✅ QEMU-verified |
| **1** | 4-level paging (PML4..PT) + W^X identity map + `CR0.WP` | ✅ QEMU-verified |
| **2** | IDT/exceptions (256 stubs) + LAPIC + periodic timer (calibrated against PIT) | ✅ QEMU-verified |
| **3** | GDT/TSS + ring 3 + `syscall`/`sysret` + context switch | ✅ QEMU-verified |
| **4** | **kernel core runs**: HAL architecture-selective, self-tests + scheduler + cap-guarded IPC | ✅ QEMU-verified |
| **4b** | **SMP** (INIT-SIPI-SIPI), ACPI MADT/MCFG, Multiboot memory plan | ✅ QEMU-verified (4 cores) |
| **4c** | **Ring 3**: user threads with syscall + fault isolation (SAS model) | ✅ QEMU-verified |
| **5** | **Per-process address spaces**: isolated PDs with their own address space | ✅ QEMU-verified |
| **6** | **PCI enumeration** (ECAM/MCFG) + **VT-d bring-up** with default block | ✅ QEMU-verified |
| 7 | VT-d per-device assignment, PCID optimisation, porting of the ARM test services | open |

### Stage 4 (ext-31): the actual microkernel

Up to stage 3 the x86 branch was a chain of **hardware demos** — the kernel core itself was not
even compiled in on this branch (`caprock-*` were aarch64-only). Since ext-31
`caprock-hal` is **architecture-selective** (`src/aarch64/` and `src/x86_64/` behind the same API),
and thus the same core runs on x86 as on ARM:

```
memtest : ALL PASS      <- identische arch-neutrale Selbsttests
zerotest: ALL PASS
captest : ALL PASS
budget  : ALL PASS
sched   : 1 Kern, 256 Thread-Slots (512 hostbar), Tabellen 208 KiB aus dem RAM
sched   : Worker-Runden [3, 3, 3]  -> ALL PASS   (LAPIC-Timer verdraengt praeemptiv)
ipc     : CALL(21) ueber Endpoint-Cap -> 42      -> ALL PASS
audit   : sched_audit=0 cdt_audit=0              -> ALL PASS
```

**The kernel core contains not a single `cfg(target_arch)`.** This is possible because both
architectures use the same **trap model**: register set into a `TrapFrame` on the
stack, the handler gets its address, the return value is the frame to be restored —
the context switch is on both sides a pure stack-pointer swap (`eret` or `iretq`).

### Stage 4b (ext-32): SMP + platform description

```
acpi    : 4 CPU(s) laut MADT
acpi    : PCI-ECAM @ 0xb0000000, Busse 0..255
mbi     : Speicherplan gelesen -> RAM bis 0x1ffe0000 (511 MiB)
smp     : 4 von 4 Kern(en) online
sched   : core 0..3 ticks=24/24/23/22   -> jeder Kern hat LAPIC-Timer + Scheduler-Instanz
```

Three things that are different (and simpler) on ARM:

- **Core start.** PSCI starts a core directly in 64 bit at an arbitrary address. On x86 there is
  no such firmware interface: the AP starts after `INIT`-`SIPI`-`SIPI` in
  **16-bit real mode below 1 MiB**. The trampoline (`hal::x86_64::power`) does
  16 → 32 → 64 bit and then jumps into the Rust entry.
- **Where the trampoline lies.** The run address is 0x8000 (so that all labels in it are absolute),
  but the **load address** is in the kernel image: a separate load segment below 1 MiB sorts the
  ELF segments by address and pushes the Multiboot header out of the first 8 KiB of the file —
  then QEMU silently does not boot at all. The BSP copies the block once.
- **W^X across the mode switch.** The AP continues at 0x8000 **after** it has
  set `CR0.PG`; if the page were NX then, there would be a #PF at exactly this moment without an IDT
  and thus a triple fault. The page is RW while copying and is afterwards set to R-X via
  `mmu::protect_page`; the parameters lie on a **separate** RW page,
  so that never one page is both writable and executable.

**Platform description** now comes from the platform instead of from the code: CPU list from the
**ACPI MADT** (counterpart to the `/cpus` nodes of the DTB), PCI ECAM window from the **MCFG**,
RAM size from the **Multiboot memory plan**.

### Stage 4c (ext-32): ring 3

```
ring3   : Ring-3-Thread machte 5 Syscalls; abgefangene Ring-3-Faults: 1
el0-trap: User-Thread faultete (FAR=0x100000) -> beendet, Kernel laeuft weiter
```

A ring-3 thread works with the kernel via `int 0x80`; a second one reads **kernel** memory
and is terminated for it, without taking the kernel with it — the x86 counterpart to the `el0iso` test.
Three things were needed for this:

- **`US` on all four levels.** On x86 the effective permission is the AND combination
  across PML4E/PDPTE/PDE/PTE. Without `US` in the intermediate levels the CPU denies every
  ring-3 access, no matter what is in the leaf. The intermediate levels are therefore permissive, the
  decision is made at the leaf — just like `AP[1]` on aarch64.
- **`TSS.RSP0` per thread.** On ARM every exception level has its own stack register; on x86
  the CPU switches to `TSS.RSP0` on a trap from ring 3. It is now set on every return
  to ring 3 to the kernel stack of **exactly this** thread.
- **Ring-3 code + data in separate sections** (`.user_text`/`.user_data`), because the
  kernel `.text` stays supervisor-only. A ring-3 thread therefore cannot call any kernel function
  — its syscall is embedded directly, as with the EL0 demos on aarch64.

### Stage 5 (ext-33): per-process address spaces

```
iso     : SAS-Thread las 0x5e141a4e0bedc0de; isolierter Thread faultete an DERSELBEN Adresse
```

Construction of an isolated address space — the same model as on ARM (**the kernel sees everything,
the user nothing**, and the PD's frames are "stamped into" this base surface), only with
one more level:

```
  PML4 (l1) ──[0]──> PDPT ──[0]──> PD (l2)   GiB 0: Kernel-Image (GETEILTE PTs, wie die
                          │                         Kernel-L3 auf ARM) + RAM supervisor-only
                          │                         + User-Blöcke dieser PD (US)
                          └─[1..3]─> ISO_PD_HIGH    GiB 1..3 supervisor-only (statisch, geteilt)
```

Compared with ARM (39-bit VA, three levels) x86 has one more table level; `vspace_create_base`
therefore gets an `alloc` back channel for the PDPT (unused on ARM). The static
`ISO_PD_HIGH` tables for GiB 1..3 save three frames **per** address space: they contain nothing
PD-specific, only "RAM, but only for the kernel".

What is **still missing** on x86 (honestly reported as "not supported", not half implemented):

- **Per-process address spaces**: the `vspace_*` functions still report `false` — PCID management
  and a per-VSpace table pool are missing. Ring-3 threads therefore run in the **SAS model**
  (shared address space, isolation against the kernel via the `US` bit) — the same as what applies on aarch64
  for *trusted* PDs. Isolated PDs (each with its own address space) do not yet exist on x86.
- **IOMMU**: instead of SMMUv3 the `NullIommuEnforcer` applies — it enforces **no** hardware isolation
  and says so (`is_active() == false`). The software-side DMA guarantees
  (ownership, bounds, revoke order, audits) are unaffected.
- **Boot archive/loader**: `SYS_LOAD` fails cleanly (the ARM window has no
  counterpart on x86; the equivalent would be Multiboot modules).
- **VT-d per-device assignment**: the bring-up blocks everything (default block, `GSTS.TES` active);
  context entries + second-level tables per domain are missing, `attach` reports `false`. Only
  afterwards can the `dma`/`virtiorng` tests of ARM be transferred.
- **PCID**: the address-space switch flushes the whole TLB (the ASID is a software identifier).
- **The ARM test services** (`threads/mod.rs`, ~5000 lines; hot reload, fuzzers, adversarial
  services, MCS tests) continue to run only on ARM; x86 has its own compact bring-up test.

## Changed/new files (vs. `master`)

- `kernel/src/arch/x86_64/mod.rs` — boot trampoline + 16550 serial + `x86_rust_entry` (stage 0).
- `kernel/x86_64-link.ld` — Multiboot1 linker (@ 1 MiB, `.boot.bss` outside the zeroed `.bss`).
- `build-x86.sh`, `test-qemu-x86.sh` — x86 build (+ELF32 cast) & QEMU test.
- `.cargo/config.toml` — `[target.x86_64-unknown-none]` (linker script, static).
- `kernel/Cargo.toml` — `caprock-*` deps aarch64-only (x86 stage 0 = only boot+serial).
- `kernel/src/{main,panic}.rs`, `kernel/src/arch/mod.rs` — `cfg(target_arch)` separation.
