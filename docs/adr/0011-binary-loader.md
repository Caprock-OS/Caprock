# ADR 0011 — Generic binary loader (externally built processes, in-kernel, cap-gated)

Status: **Proposed** (ext-26; architecture + format decided by the user, implementation follows after plan approval)

## Context

Until ext-25, *every* process/PD code is **compiled into the kernel image** (`.user_text` section)
and spawned via **in-kernel function pointers** (`spawn` = EL1/TrustedSAS, `spawn_user`/
`spawn_isolated*` = EL0/isolated). There is **no** mechanism to load externally built code:
no syscall, no QEMU `-device loader` reading, no module table, no loader.

Goal of ext-26: the transition from **embedded processes** to a **real microkernel**, in
which TrustedSAS, HardwareLand and UserLand processes are **built externally, loaded and started**.
For this the kernel provides a **generic binary loader**; the existing
security architecture (capabilities, region runtime, DMA, hot reload, domains, audits) remains
**fully preserved** and is integrated into the loader.

## Decision

### 1. Placement: **in-kernel loader, cap-gated** (architecture A)
The loader is kernel code (small, auditable mechanism). *Loading* is **not an
ambient right**: it requires a **`Loader` capability** (new `ObjectKind::Loader`), presented via
a new syscall `SYS_LOAD`. The loader grants **nothing** to itself; the
loaded process receives exclusively caps that the **caller** (loader cap holder) already
holds and names in the manifest for delegation — installed via the **existing**
`install_cap_checked` (domain policy + audits apply unchanged).

*Comparison (see below):* A instead of B (userspace loader server), because B requires many new, extremely
powerful syscalls (foreign-PD factory + foreign-cap install) — which **enlarges** the attack surface
we want to minimise — and needs an in-kernel bootstrap loader anyway. The only
TCB growth of A is the ELF parser; it is kept minimal (safe Rust, bounds-checked, own
fuzzer).

### 2. Format: **minimal ELF64, only `PT_LOAD`, no relocations**
Statically linked Rust binaries as `ET_EXEC` with a **fixed load VA**. The loader implements ONLY:
validate the ELF64 header (magic, class=64, data=LE, machine=`EM_AARCH64`=0xB7, type=`ET_EXEC`),
read the program header table, copy **`PT_LOAD` segments** (p_offset/p_filesz/p_memsz/p_vaddr/p_flags),
zero `.bss` (memsz>filesz), jump to the entry point. **No** dynamic linking, **no**
relocations, **no** symbol/section parsing. W^X from `p_flags`: `X`→RX, `W`→RW, otherwise RO.

### 3. Boot delivery: **boot archive in a reserved RAM window**
QEMU `-device loader,file=boot-archive.bin,addr=MOD_BASE` places ONE archive in a **window reserved
at the top of RAM** (`MOD_BASE = ram_end - MOD_WINDOW`). `init_mem` gives the `PhysAllocator`
only `[free_base, MOD_BASE)` — the window is thus **never** allocatable (no conflict). Archive:
`Header{ magic, version, count, reserved }` + `count ×` `Entry{ name[16], blob_off, blob_len,
domain, manifest_off, manifest_len, flags, hash[32] }` + blobs + manifests. A build script
(`tools/mkarchive`, **no** kernel code) assembles the archive from the externally built binaries.
The kernel reads the archive (safe Rust, bounds-checked) and loads entries.

### 4. Domains: ALL loaded processes run EL0-isolated (correction, "TrustedSAS also in EL0")
- **Loaded processes are ALWAYS EL0-isolated.** `load_into_pd` always spawns an EL0 thread in
  its own VSpace (segments mapped at `p_vaddr`, W^X). A malicious/faulty binary is
  therefore **hardware-isolated** (a fault kills only the process) — **independent of the domain**.
- The **domain** fixes only the **cap authority**, NOT the privilege level:
  - **UserLand:** no hardware/management rights.
  - **HardwareLand:** hardware caps + exactly one trusted partner (pre-created backend PD with
    channel; via `load_program_into_pd`).
  - **TrustedSAS:** may hold `PdControl`/`Loader` caps (trust level), but runs **loaded
    EL0-isolated** — NOT EL1. `domain_audit` already allows isolated TrustedSAS PDs.
- **Consequence:** there is **no** privilege-based loading prohibition. The earlier reservation ("externally
  loaded EL1 code undermines SIP") lapses, because **no** loaded process runs at EL1. Thus
  **all three domains** can safely be loaded externally (precondition for the ext-27 test services). The
  existing **EL1-global** TrustedSAS PDs (in-kernel, SAS, zero-copy) remain unchanged; that
  is a separate, in-kernel configuration. `verify_image`/`prog.hash` remain as an
  **integrity/signature hook** (for future trusted production services), but no longer gate
  by domain.

### 5. Capability initialisation via manifest
Every program carries a **manifest** (in the archive): `domain`, `entry_extra` (start parameters),
**cap endowment list** = references to **caps of the caller** that are delegated into the new PD
(slot → right). The loader: `create_pd_in_domain(domain)` → for every entry `copy/mint` from the
caller cap (never more rights than the caller) → `install_cap_checked(new_pd, slot, cap)`
(domain policy applies) → spawn the entry thread, **start parameter in `x0`** (pointer to a
boot info structure in a region owned by the process). **No escalation, no
domain bypass** — structurally guaranteed by `install_cap_checked`.

### 6. Lifecycle: start / end / hot reload / several processes
`SYS_LOAD` returns a **`PdControl` cap** to the new PD (if the caller is TrustedSAS)
→ start/stop/pause/resume via the **existing** `SYS_PDCTL`. **Hot reload:** reloading a program from
a newer archive version = load a new PD instance + `reload_swap` (existing ext-7
mechanism, same endpoint). Several processes: the loader is reentrant (one PD per call;
region runtime + VSpace allocator support the multiple use).

### 7. Signature + versioning: **prepared**, not enforced
Archive/manifest headers carry `version` and a `hash[32]` field (reserved). Verification is
a clearly marked hook (`verify_image(entry) -> bool`, for now `true` except for EL1 = mandatory
once implemented). Thus signature checking + version management can be retrofitted later **without
a format/API break**.

## Refinements (user review, incorporated before L1)

1. **Loader as its own kernel service, small public API.** The loader lives in the kernel, but is
   its own module (`kernel/src/loader.rs` + crate `caprock-loader`) with a **minimal** public
   API: at its core a single operation `loader::load_image(&Program, …) -> Result<Pd, LoaderError>`.
   Thus it stays formally analysable + later exchangeable.
2. **Parse ELF entirely in safe Rust.** All header/offset/size checks live in the crate
   `caprock-loader` (`#![forbid(unsafe_code)]`). `unsafe` arises **only** when copying already
   **validated** segments into the target memory (kernel glue) — never in the parser.
3. **Source exchangeable — API NOT archive-dependent.** The boot archive is only the **first** source.
   The internal loader API speaks a **source-agnostic** `Program` descriptor (metadata + raw
   image bytes), not the archive format. Future sources (file system, flash, network) deliver
   the same `Program` → `load_image` stays unchanged. The archive parser is only **one** producer.
4. **Stable numeric program ID.** Every entry carries a **`program_id`** (survives
   name changes) in addition to `name`, `version`, `domain`, `hash`. IDs are stable for
   hot reload, logs, debugging; names are only human-readable.

## Architecture comparison

| Criterion | **A: in-kernel, cap-gated** ✅ | B: userspace loader server | C: hybrid |
|---|---|---|---|
| ELF parse | in the kernel (TCB; minimal+fuzzer) | in userspace (bug isolated) | selectable in kernel/root |
| New syscalls | **0–1** (`SYS_LOAD`) | **many, close to god mode** | 1 |
| New attack surface | small | **large** (PD factory + foreign-cap install) | medium |
| Bootstrap | none | in-kernel bootstrap needed | medium |
| "no special rights" | loading cap-gated, caps via checked path | purest reading | cap-gated |
| Complexity | low | high | medium |

**Chosen: A.** B is philosophically purer (parser out of the TCB), but buys that with syscalls that
create foreign PDs and install arbitrary caps into foreign PDs — exactly the powerful primitives
that negate the TCB reduction. A keeps the TCB growth limited to a minimal, fuzzable
ELF parser and uses the existing cap/domain-checked paths.

## Security model / invariants

- **Loader authority:** without a `Loader` cap no `SYS_LOAD` (`ERR_BADCAP`). The loader grants only
  caps that the caller holds (delegation, no creation out of nothing).
- **Domain policy unchanged:** all initial caps via `install_cap_checked`; domain fixed and
  immutable at PD creation; `domain_audit` continues to apply (HW only HardwareLand, PdControl
  only TrustedSas, untrusted isolated).
- **W^X:** loaded code segments RX, data RW/RO — `vspace_audit` continues to check (no
  EL0 page W+X).
- **Region balance:** loaded segment regions via the region runtime; teardown returns all
  (`churn`/balance audit applies).
- **New `loader_audit`:** loaded images consistent — segments disjoint, inside the
  PD regions, W^X correct, domain == manifest, no segment overlaps the kernel image/other PDs.
- **New loader fuzzer:** **faulty ELFs/archives** (truncated, bad offsets, memsz<filesz,
  overlapping segments, huge memsz, bad entry, W^X violation in the manifest, bad magic) must be
  **rejected** **without** compromising the kernel (no panic, no OOB).
- **EL1 reservation:** TrustedSAS loading only signature-verified (once implemented); until then
  EL1 loading is rejected by default.

## Consequences

- **New structures outside the kernel:** `programs/{trusted,hardware,userland}/…` and
  `tests/{trusted,hardware,userland}-test-N/` as **independent** Cargo projects (own
  target/linker, own build/docs/tests, no cross-dependencies). A common, minimal
  **SDK crate** (`libcaprock`: syscall stubs, `_start`/crt0, panic handler, boot info) is a
  *common* dependency, **not** a mutual one between services.
- **Kernel change (sanctioned):** ELF loader module, `ObjectKind::Loader`, `SYS_LOAD`,
  boot archive reader, allocator window reservation, `loader_audit` + fuzzer. The existing
  microkernel semantics (IPC, caps, domains, DMA, hot reload) remain **unchanged**; the loader
  only *uses* them.
- **Deliberately deferred:** relocations/static PIE (for arbitrarily placeable TrustedSAS),
  real signature/hash verification, run-time archive update via IPC, demand paging.
- **Important honesty (test environment):** kernel-only cap operations (copy/mint/install/spawn/
  revoke) are callable from **no loaded process** (no syscall, no linkage) — not even
  from EL1 TrustedSAS binaries. Adversarial **cap/CDT attacks** therefore remain in the
  **in-kernel self-test suite**; loaded EL0 processes attack via the **syscall ABI**
  (invalid cap indices, wrong rights, IPC to non-owned endpoints, MAP onto foreign memory,
  PDCTL without authority, flooding, races). That is the correct, honest delimitation.
