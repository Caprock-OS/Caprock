//! Boot trampoline for aarch64 / QEMU `virt`.
//!
//! Entered directly by QEMU (`-kernel`) at EL1 with `x0` = DTB pointer. The
//! trampoline:
//!   1. masks all exceptions,
//!   2. parks every secondary core (Phase 0 is single-core),
//!   3. installs the boot stack,
//!   4. zeroes `.bss`,
//!   5. branches to `kernel_main(dtb)`.
//!
//! This is the only assembly in the kernel and lives in a permitted unsafe
//! domain (early boot / CPU bring-up). `x0` is preserved end-to-end so the DTB
//! pointer reaches `kernel_main` as its first argument.

use core::arch::global_asm;

global_asm!(
    r#"
.section .text.boot, "ax"
.globl _start
_start:
    msr     daifset, #0xf             // mask Debug, SError, IRQ, FIQ

    mrs     x9, mpidr_el1             // isolate affinity Aff0..Aff2
    movz    x10, #0xffff
    movk    x10, #0xff, lsl #16
    and     x9, x9, x10
    cbz     x9, 2f                    // affinity 0 -> primary core; else park
1:
    wfe
    b       1b

2:
    mov     x9, #(1 << 20)            // CPACR_EL1.FPEN = 0b01: FP/SIMD nur an EL0
    msr     cpacr_el1, x9             // trappen (Lazy-FP); EL1-Kernel ist soft-float
    isb

    adrp    x9, __boot_stack_top      // install boot stack (SP must be 16-aligned)
    add     x9, x9, #:lo12:__boot_stack_top
    mov     sp, x9

    adrp    x9, __bss_start           // zero .bss: [__bss_start, __bss_end)
    add     x9, x9, #:lo12:__bss_start
    adrp    x10, __bss_end
    add     x10, x10, #:lo12:__bss_end
3:
    cmp     x9, x10
    b.hs    4f
    str     xzr, [x9], #8
    b       3b
4:
    bl      kernel_main               // x0 still holds the DTB pointer
5:
    wfe                               // kernel_main is `-> !`; this is a guard
    b       5b

// Secondary cores enter here via PSCI CPU_ON. PSCI passes the context-id in x0;
// we use it as the stack top for this core (MMU is still off — see
// kernel_secondary_main, which enables it first). DTB is not needed here.
.globl _start_secondary
_start_secondary:
    msr     daifset, #0xf             // mask exceptions during bring-up
    mov     x9, #(1 << 20)            // CPACR_EL1.FPEN = 0b01: FP/SIMD nur an EL0 trappen
    msr     cpacr_el1, x9             // (wie Primärkern; EL1 ist soft-float)
    isb
    mov     sp, x0                    // x0 = context-id = this core's stack top
    bl      kernel_secondary_main
6:
    wfe
    b       6b
"#
);

// ---------------------------------------------------------------------------------------------
// UEFI entry (`boot/uefi-aarch64` stub). Additive: `_start` above is the `-kernel` path and is
// not modified. The stub jumps here with `x0` = physical address of a `UefiHandover`, MMU and
// caches possibly ON (firmware state), exceptions masked, at EL2 or EL1 — nothing is assumed.
//
//   1. detect CurrentEL; at EL2 configure HCR_EL2/CNTHCTL_EL2/CPTR_EL2/... and `eret` to EL1h,
//   2. at EL1 turn the firmware's MMU and caches off (the kernel's own bring-up expects the
//      `-kernel` state: MMU off), invalidate TLB + I-cache,
//   3. same boot stack / .bss / FP setup as `_start`, then `kernel_main_uefi(handover)`.
//
// Only the primary core runs the UEFI firmware's handoff; secondaries are started later via PSCI
// at `_start_secondary` exactly as on the `-kernel` path.
// ---------------------------------------------------------------------------------------------
global_asm!(
    r#"
.section .text.boot_uefi, "ax"
.globl _start_uefi
_start_uefi:
    msr     daifset, #0xf
    mov     x19, x0                   // handover pointer survives everything below (callee-saved)

    mrs     x9, CurrentEL
    lsr     x9, x9, #2
    and     x9, x9, #3
    cmp     x9, #2
    b.ne    2f                        // EL1 (or anything else we cannot drop from): stay

    // ---- EL2 -> EL1 -------------------------------------------------------------------
    mov     x9, #(1 << 31)            // HCR_EL2.RW = 1: EL1 is AArch64; no traps, no stage 2
    msr     hcr_el2, x9
    mrs     x9, cnthctl_el2
    orr     x9, x9, #3                // EL1PCTEN | EL1PCEN: EL1 may use the physical counter/timer
    msr     cnthctl_el2, x9
    msr     cntvoff_el2, xzr
    mov     x9, #0x33ff               // CPTR_EL2: RES1 bits only -> no FP/SIMD/trace traps
    msr     cptr_el2, x9
    msr     hstr_el2, xzr
    msr     mdcr_el2, xzr
    mrs     x9, midr_el1
    mrs     x10, mpidr_el1
    msr     vpidr_el2, x9             // EL1 reads the real IDs
    msr     vmpidr_el2, x10
    mrs     x9, id_aa64pfr0_el1
    ubfx    x9, x9, #24, #4           // GIC system-register interface present?
    cbz     x9, 1f
    mov     x9, #0x9                  // ICC_SRE_EL2: SRE | Enable (let EL1 use ICC_SRE_EL1)
    msr     S3_4_C12_C9_5, x9         // icc_sre_el2
    isb
1:
    movz    x9, #0x0800               // SCTLR_EL1 reset value: RES1 bits, MMU/caches off
    movk    x9, #0x30d0, lsl #16
    msr     sctlr_el1, x9
    mov     x9, #0x3c5                // SPSR_EL2: EL1h, D A I F masked
    msr     spsr_el2, x9
    adr     x9, 3f
    msr     elr_el2, x9
    isb
    eret

    // ---- EL1: switch the firmware MMU/caches off ------------------------------------------
2:
    movz    x9, #0x0800
    movk    x9, #0x30d0, lsl #16
    msr     sctlr_el1, x9
    isb
3:
    tlbi    vmalle1
    ic      iallu
    dsb     sy
    isb

    mrs     x9, mpidr_el1
    movz    x10, #0xffff
    movk    x10, #0xff, lsl #16
    and     x9, x9, x10
    cbz     x9, 5f                    // firmware hands off on its boot core, normally affinity 0
    // A boot core that is not affinity 0 is not supported (PSCI/secondary numbering assumes 0).
4:
    wfe
    b       4b
5:
    mov     x9, #(1 << 20)            // CPACR_EL1.FPEN = 0b01, as in `_start`
    msr     cpacr_el1, x9
    isb
    adrp    x9, __boot_stack_top
    add     x9, x9, #:lo12:__boot_stack_top
    mov     sp, x9
    adrp    x9, __bss_start
    add     x9, x9, #:lo12:__bss_start
    adrp    x10, __bss_end
    add     x10, x10, #:lo12:__bss_end
6:
    cmp     x9, x10
    b.hs    7f
    str     xzr, [x9], #8
    b       6b
7:
    mov     x0, x19
    bl      kernel_main_uefi
8:
    wfe
    b       8b
"#
);
