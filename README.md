# Caprock

A from-scratch, capability-based **microkernel in Rust**, inspired by **seL4** and the
**seL4 Microkit** runtime — not an seL4 fork. It runs on **x86_64 and aarch64**; both build from
this branch (`arch/x86_64`, the only branch published upstream), x86_64 being the primary target.

**Target picture:** the kernel is the **base system of a cloud server** — no hypervisor beneath it,
no VM inside it. Isolation, time and resource management are done by the kernel itself. What follows
from that and what is still missing is in [`todo.md`](todo.md) (section Z) and
[`docs/plan-betriebsbereit.md`](docs/plan-betriebsbereit.md).

Goals: highly secure · high-performance · capability-based · deterministic · fully modular.

> **Documentation language.** Everything new is written in English. Most of the existing
> documentation (`todo.md`, `done.md`, `docs/`, comments in older modules) is still German and is
> deliberately **not** retranslated — its precision is its value. See `CLAUDE.md` for the rule.

## The long-term goal — and the claim it supports

**Caprock OS is meant to end up written entirely in [Gabbro](https://github.com/SimonVitzethum/Gabbro)
— kernel, drivers, desktop — and formally verified.** Gabbro is a systems programming language with
a Rust compiler and a Lean 4 model of its core language and safety properties; see its repository
for what is proved and what is not. Alongside the verified part, unverified code runs for
usability: Linux drivers and binary compatibility.

The split is **per property**, not global:

| | in Gabbro, verified | unverified, confined |
|---|---|---|
| **what** | kernel, isolation mechanics, compositor core, own drivers | Linux drivers in PDs, Linux binaries via the personality layer, foreign applications |
| **promise** | proved against a specification, under named assumptions | **cannot break out** — IOMMU, capabilities, PD boundary |

The defensible form of the promise is **not** "everything is proved, therefore safe", but
**"the unproved cannot break out."** A proof holds against a *specification*, under assumptions about
the compiler, the hardware, DMA and initialisation — and a wrong specification is proved along with
the rest. This implies the order of work: **confinement first, then proof.** A Linux driver in an
IOMMU-bounded PD carries the promise today; a proof about it will never exist.

Caprock itself stays in Rust until that port happens; nothing below is written in Gabbro yet.

## Quick start

```sh
# x86_64 (QEMU q35, Multiboot)
./build-x86.sh --features selftest
./test-qemu-x86.sh                   # full suite
RUNS=8 ./test-qemu-x86.sh            # boot eight times and report the rate

# aarch64 (QEMU virt)
./build.sh && ./run-qemu.sh          # build + boot (quit: Ctrl-A X)
./test-qemu.sh                       # full suite (PASS/FAIL)
```

Prerequisites: `rustup` with **nightly** (+ `rust-src`, because of `-Z build-std`),
`qemu-system-x86_64` and/or `qemu-system-aarch64`, `python3` with `cryptography` (for signing). The
toolchain is pinned in `rust-toolchain.toml`. x86 specifics are in [`README-X86.md`](README-X86.md).

**Runs from a fresh clone.** `keys/` is gitignored; the suites generate missing **test keys** on the
first run and rebuild the kernel afterwards (the key database is compiled into it). A private key in
the repository would not be a test key in an open-source project but a published one — the price of
this decision is a machine-local value in the image, which costs reproducibility *between*
developers, not *within* one checkout.

## Memory model — and what it does NOT cover

Two isolation paths, deliberately kept apart:

* **TrustedSAS** — single address space, no address-space switch. Safety arises *intralingually*
  (safe Rust cannot forge a pointer into foreign memory) plus capabilities for authority. A
  certificate gate enforces the precondition (`unsafe_status == ALL_PASS`, ADR 0014).
* **Isolated PDs** — one address space per PD, hardware isolation, optionally **cache-colour
  partitioned**.

> **Fixed rule: TrustedSAS is for our own code only. No customer code ever runs in a TrustedSAS PD.**
> Intralingual safety can only be checked against *source code*, never against a foreign binary.
> Everything foreign gets the isolated path — which is therefore the product path.

## Status

Numbers carry a date **and** a sample size; the authoritative, current record is `CLAUDE.md`,
[`done.md`](done.md) and [`todo.md`](todo.md). As of 2026-08-26:

* **Both architectures** share one kernel core: capability system with derivation tree, preemptive
  scheduler, capability-guarded IPC, protection domains, hot-reload of a server component behind the
  *same* endpoint cap, signed boot archive and manifest, root task, `SYS_SPAWN`, a per-PD capability
  budget. Capability system, scheduler and IPC carry **no `cfg(target_arch)`**; what differs lives in
  `kernel/src/arch/`.
* **x86_64:** 4-level paging with W^X, SMP via INIT-SIPI-SIPI, x2APIC, LAPIC timer, PCI via ACPI
  MCFG, **VT-d** (bring-up, capabilities, DMAR evaluation with grouping), Multiboot modules as start
  set. Main and loader suites report `== ALL PASS ==` at 512M · 2560M · 3G · 6G of RAM.
  A 50 000-run measurement after the D0 fix found 0 start-race hits (9 in 50 000 before) — with a
  stated caveat about load that is recorded in `CLAUDE.md`.
* **aarch64:** SMMUv3/DMA hardening, 8 cores with MMU/W^X. 6 of 6 runs with identical signature on
  2026-08-13; one rare open hang class (D13) is tracked in `todo.md`.
* **Drivers outside the kernel:** virtio transport, block and network device as driver PDs
  (`crates/caprock-virtio`, `programs/hardware/virtio-blk`), two driver PDs at once, each with its
  device named in the manifest, replaceable at run time. Above the sector sits a storage stack that
  is entirely outside the kernel: GPT and FAT16 parsers (`forbid(unsafe_code)`, host-tested) and a
  file-system PD that reads and writes. The DMA path is measured end to end on x86 through VT-d,
  including negative cases (one device cannot reach another PD's DMA region).
* **Cache partitioning between PDs** (`todo.md` A1): the LLC geometry is *measured* (CPUID leaf 4 /
  CLIDR/CCSIDR), and two isolated PDs get disjoint colour sets — region, kernel stack and page
  tables. What this does **not** cover is in [`docs/invariants.md`](docs/invariants.md) §12; the main
  limit: no colour helps against sibling hyperthreads. Colouring only works on bare metal, not as a
  guest.
* **Checkpointing:** a thread survives a **boot boundary** (Z4 stage 2); a capability that cannot be
  carried over prevents the checkpoint before it is written.
* **Verification:** 16 Verus proof files with 0 errors, plus three model-fidelity guards (cap space,
  IPC, scheduler); Kani and Loom harnesses; host tests for the pure crates (`tools/host-tests.sh`).
  What the proofs say about a property is limited to what the model contains — the guards exist to
  catch the model drifting from the code.
* **Core hand-over (variant B):** a Linux kernel module takes cores offline and brings them into long
  mode with their own GDT/CR3 — repeatable, without reboot (`tools/handover/`).

### What is still missing

Not hidden: **VT-d interrupt remapping** has no IRTE assignment yet (the table is all
"not present" on purpose), so `CAP_IRQ` for drivers is open and every driver **polls**; without it no
device may go to a tenant. The device *offer* is still virtio-bound (the choice is in the manifest,
the offer is not). The per-PD capability space is a fixed 16 slots (`NCAPS`), which is the real cap
on a capability budget. Migration across server boundaries is design, not code. The full list is in
[`todo.md`](todo.md).

## Building without the test infrastructure

About half of the compiled code is test infrastructure — self-tests, demo threads, DMA/colour tests,
the harness. It sits behind the `selftest` feature (`.text` 0x25000 → 0x11000, i.e. 54 % smaller):

```sh
# without test infrastructure
cargo build --release --no-default-features --target x86_64-unknown-none -p caprock-kernel
# with
cargo build --release --features selftest   --target x86_64-unknown-none -p caprock-kernel
```

The test suites request `--features selftest` for the booted image **explicitly** instead of taking
it through `default` — otherwise flipping the default would turn them silently quiet, and quiet looks
like success. **Both** configurations are built: a build nobody builds rots silently. It is also
checked that `.text` really shrinks without the feature; otherwise the gating could have become
ineffective unnoticed.

## Project boundaries

The boot image contains exactly two things: the **kernel** and **one manifest file** that says what
gets loaded. Drivers, network stack, file systems and services live outside and are replaceable at
run time. The manifest is an **authority document**, not a configuration file: it fixes the entire
initial distribution of authority and is therefore signed and bound to the kernel image.

## Documentation

- [`todo.md`](todo.md) — open items only; **section Z: the target architecture** everything is
  measured against
- [`docs/plan-betriebsbereit.md`](docs/plan-betriebsbereit.md) — order of work and reasons
- [`done.md`](done.md) — finished work *with the assumptions that turned out to be wrong*
- [`docs/invariants.md`](docs/invariants.md) — the load-bearing guarantees **and their limits**
- [`docs/00-overview.md`](docs/00-overview.md) — structure, crate layout, trust model
- [`docs/adr/`](docs/adr/) — architecture decisions
- [`docs/verification.md`](docs/verification.md) — Kani/Loom/Verus, state and reach
- [`docs/grenze.md`](docs/grenze.md) — which crates must become permissive (see License)
- [`CLAUDE.md`](CLAUDE.md) — entry point with the measured state and the trap register
- [`AGENTS.md`](AGENTS.md) / [`STATUS.md`](STATUS.md) — division of labour and running state

## Spelling of the name

**`Caprock`** in prose, **`caprock`** in crate names, paths and identifiers. Exactly one spelling per
place, no variants — no `CapRock`, no `CAPRock`, no `capRock`.

The rule lives here rather than in a style guide because this project has already paid for the drift:
`seL4` became `SEL4` became `Sel4lake`, and in the end `SEL4Lake`, `SeL4Lake` and `sel4lake` stood side
by side in the tree. The rename had to replace **case by case**, across 241 files and 20 crate
directories — a blanket replace would have hit something different at every variant. A spelling rule
that appears only after the first variants comes too late: it describes the stock instead of
preventing it.

**`CAPROCKS` is explicitly NOT a spelling variant but a data value.** It is the **8-byte magic** the
test suites put in sector 0 of the disk image and the kernel checks against — cross-checked in three
places: `tools/mkgpt.py` writes it, the kernel computes against it, both QEMU suites depend on it.
Eight characters, because the **width** matters in exactly those three places; a seven-character
`CAPROCK` would have broken the width check without "name" appearing anywhere. The value does not
follow the spelling rule and does not define it — treat it separately, as last time.

**OPEN — whether the spelling with K ("Kaprock") should apply is not finally decided.** If it does,
it costs two things: another complete rename pass **and** a new availability check — crates.io name
`kaprock` with prefix `kaprock-`, plus the trademark register. The check that ran for `caprock` does
not cover `kaprock`; a free name says nothing about another one. Currently **Caprock** is realised,
and so is the GitHub repository's name.

## License

Caprock is licensed under **AGPL-3.0-or-later** (`LICENSE`).

**§13 ("Remote Network Interaction") is the point of the choice:** anyone who offers a **modified**
version over a network must offer its source to the users — the duty arises on **operation**, not
only on distribution. A customer who runs their **own program** as a PD on top is **not** covered
(see the ABI exception).

On top comes an **additional permission** under GPLv3 §7: the **ABI exception** in
`LICENSE-EXCEPTION.md`. A program that talks to the kernel only through the published system
interface is not a derived work for that reason alone. It applies **to everyone, without limit and
without application** — commercial and non-commercial alike. The same construction Linux chose for its
system interface; the copyleft character of the kernel is untouched. **Under the AGPL it is
load-bearing, not convenient** — it is what keeps §13 away from customer code.

Which crates have to become permissive for this is in `docs/grenze.md`.
