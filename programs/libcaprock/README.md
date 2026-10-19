# libcaprock — userland SDK (ext-26)

Minimal SDK for externally loaded Caprock EL0 programs: the **syscall stubs** + a **panic
handler**. Deliberately **without** a dependency on the kernel workspace (the ABI constants are duplicated) —
an externally built program depends only on this SDK.

## Syscall ABI

`svc #0`, registers: `x0`=number, `x1`=local cap index, `x2..x5`=message, `x6`=tag. Return:
`x0`=result, `x1`=badge, `x2..x5`=reply, `x6`=tag. On `eret` the kernel restores all
registers except `x0..x6`.

## API

| Function | Syscall | Purpose |
|---|---|---|
| `invoke(nr, cap, msg, tag) -> Ret` | — | raw syscall |
| `signal(cap, badge)` | `SIGNAL` | signal a notification (badge = **cap badge**, not the argument) |
| `wait(cap) -> u64` | `WAIT` | wait for a notification (returns the accumulated badge) |
| `call(cap, msg) -> Ret` | `CALL` | synchronous RPC (send + wait for reply) |
| `recv(cap) -> Ret` | `RECV` | wait for a call (server) |
| `reply(cap, msg)` | `REPLY` | answer the last caller |
| `yield_now()` | `YIELD` | voluntary relinquishing of the time slice |
| `park() -> !` | `PARK` | block oneself permanently |
| `exit() -> !` | `EXIT` | terminate oneself (stack/TCB/pool slot returned) |

Further syscall numbers (`MAP`/`UNMAP`/`PDCTL`/`LOAD`/`KILL`) are defined in `sys::*`; higher
wrappers follow as needed. A loaded process can only use caps that the loader endowed it with (via
manifest/delegation, cap-gated).

## Entry

Every program defines `#[no_mangle] extern "C" fn _start(arg: usize) -> !`. The kernel starts
it with SP set (own stack in the isolated VSpace) and passes a boot info in `x0`.
This SDK's panic handler parks silently (no heap/console in the EL0 program).
