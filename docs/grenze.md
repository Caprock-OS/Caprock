# The boundary crates — what has to become permissive, and why exactly these

**As of: 2026-08-09. IMPLEMENTED** — all seven crates carry `MIT OR Apache-2.0`. The core remains
`AGPL-3.0-or-later`.

Caprock is `AGPL-3.0-or-later`. Everything that a **program outside the kernel** links is thus
a licence boundary: a PD that builds against a GPLv3 crate is linked with GPLv3 code — and then
neither process separation nor the ABI exception helps, because both concern the *interface*,
not the *linking*.

This is the same boundary for which Linux has the **UAPI header exception**.

## Measured, not estimated: what programs link today

| Program | links |
|---|---|
| `init`, `hello`, `svc-demo` | `libcaprock` |
| `wasmhost` | `libcaprock`, `wasmi` |
| `fs` | `libcaprock`, `caprock-part`, `caprock-fat` |
| `virtio-blk` | `libcaprock`, `caprock-virtio`, `caprock-part`, `caprock-dma` |
| `virtio-net` | `libcaprock`, `caprock-virtio` |

## Must become permissive (`MIT OR Apache-2.0`)

| Crate | why |
|---|---|
| `programs/libcaprock` | **the SDK. Every program links it** — without this line everything else is moot |
| `crates/caprock-abi` | syscall numbers, result codes, register assignment. **The ABI itself** |
| `crates/caprock-dma` | `DmaPool`/`DmaBuf` — every driver PD relies on it |
| `crates/caprock-wait` | the `Park` trait, mutex/completion — every PD with several threads |
| `crates/caprock-virtio` | `Region`, descriptor typestate, transport — every virtio PD |
| `crates/caprock-part` | GPT parser, in service PDs |
| `crates/caprock-fat` | FAT16 parser, likewise |

**Seven.** All seven are already dependency-free or depend only on each other — the
switch is one line per `Cargo.toml` plus a licence notice, not a rework.

## Stays GPLv3 (core and core-adjacent)

`kernel`, `caprock-sched`, `caprock-ipc`, `caprock-microkit`, `caprock-hal`, `caprock-cap`,
`caprock-mem`, `caprock-slab`, `caprock-loader`, `caprock-trust`, `caprock-region`,
`caprock-sync`, `caprock-dtb`.

No program outside the kernel links any of these — that is the test that the boundary lies
in the right place, and `tools/kernel-grenze.sh` already holds it against the
source.

## The time is now

Relicensing needs the consent of **all** authors. As long as that is one person, it is a
commit; after the first merged outside contribution it is a collection process — and then the same
applies to every future ABI exception (see `LICENSE-EXCEPTION.md`).

**Implemented on 2026-08-09.** Under the AGPL this is no longer merely clean but load-bearing: §13 ties
the copyleft duty to **operation**, and if the boundary crates were under the AGPL, every PD would be linked
with AGPL code — the ABI exception would run empty, and every customer workload would stand under §13 suspicion.
