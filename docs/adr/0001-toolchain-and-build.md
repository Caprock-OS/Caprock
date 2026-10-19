# ADR 0001 — Toolchain, target and build

**Status:** accepted (phase 0) · **Date:** 2026-06-23

## Motivation

A bare-metal aarch64 kernel in Rust needs: a `no_std` environment, a
cross-compilation target definition, a reproducible toolchain and a
boot path that loads directly under QEMU without a bootloader SDK.

## Alternatives analysed

### A) Own kernel via `qemu -kernel` (chosen)
Own boot assembly, own linker script, loading of the ELF directly by QEMU.

- **+** Fully independent of the seL4/Microkit SDK; Caprock is a
  standalone kernel, not a PD on a foreign kernel.
- **+** Minimal moving parts, fast boot, simple tests.
- **−** We have to set up trap vectors, timer, GIC, MMU ourselves (the goal anyway).

### B) Reuse the Microkit loader (like `microkit_rust`)
The existing `microkit_rust` implementation builds userland PDs *on top of* the
seL4 kernel using the Microkit SDK and `loader.img`.

- **+** Existing build mechanics (build-std, custom target) are proven.
- **−** Presupposes an underlying seL4 kernel — contradicts the goal
  of a standalone kernel. **Rejected** as a kernel base (but a valuable
  reference for `no_std`/IPC/build conventions).

### C) U-Boot / EFI boot
- **−** Unnecessary complexity for the QEMU `virt` target environment. Rejected.

## Decision

- **Toolchain:** rustup `nightly` (here 1.98.0), pinned via `rust-toolchain.toml`
  with the components `rust-src`, `rustfmt`, `llvm-tools`. Nightly is needed for
  `-Z build-std` (there is no precompiled `core`/`alloc` for a
  freestanding target). `/usr/bin/cargo` is a
  rustup proxy on this system, so the pinning takes effect automatically.
- **Target:** custom JSON spec `targets/aarch64-caprock.json`
  (`llvm-target: aarch64-unknown-none`, `panic-strategy: abort`,
  `relocation-model: static`, `+strict-align`, `rust-lld` as linker). Newer
  nightlies additionally require `-Z json-target-spec` (in `.cargo/config.toml`).
- **build-std:** `core, alloc, compiler_builtins` with
  `compiler-builtins-mem` (supplies `memcpy`/`memset` etc. without libc).
- **Linker script:** `kernel/linker.ld`, load address `0x4008_0000` (QEMU `virt`
  convention; RAM from `0x4000_0000`). Three LOAD segments (RX/R/RW), `.bss` as
  `NOLOAD`, 64 KiB boot stack in the `.bss` area.
- **FP/NEON:** stay enabled. rustc ties `fp-armv8` to `neon` in the ABI;
  disabling is being phased out. The kernel itself uses no
  floating point; FP/SIMD state is only handled (lazily) at context switch.

## Security implications

`panic = abort` (no unwinding → no landing pads, smaller attack surface).
`relocation-model = static` (no GOT/PLT indirection at run time).
`+strict-align` is necessary as long as the MMU is off (all accesses are
device-typed and must be aligned) — see [ADR 0002](0002-no-virtual-memory-sas.md).

## Performance implications

`build-std` later allows building `core` with the same CPU features/opt level as the
kernel (e.g. a specific `target-cpu`). LTO is enabled as `thin`.
As long as the MMU is off (phase 0 only), everything runs uncached and thus slowly —
in phase 1 an identity map with enabled caches is introduced.

## Building & starting

```sh
./build.sh        # rustup run nightly cargo build --release
./run-qemu.sh     # qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp 8 -m 4G ...
```

Artefact: `build/target/aarch64-caprock/release/caprock-kernel.elf`.
