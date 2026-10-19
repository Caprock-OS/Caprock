# hal::debug draft (strand 10, patch text for HAL ownership)

As of 2026-09-10: DEBUG 33 (WRITE_MEM) is wired in the kernel, 34 (SINGLE_STEP)
and 35 (HWBREAK) fail closed (`ERR_BADSYS`, "request ok, path missing"). What is missing
is the HAL side. NOT applied (the HAL is B-owned) — for review/application
by B. Full code in the strand-10 report (session log 2026-09-10).

## Placement (proposal)

NEW `crates/caprock-hal/src/x86_64/debug.rs` + `aarch64/debug.rs`, facade
arch-neutral (`pub mod debug`, `lib.rs` re-export like `cpu`/`exception`).
Reason: DRx vs. DBGBVR/CR differ, the API surface is common.

## Contents x86 (sketch)

- `RFLAGS_TF = 1<<8` (check SDM Vol.1 §17.3!), `HWBREAKS = 4`,
  `DR7_L = [1<<0, 1<<2, 1<<4, 1<<6]` (Lx only, check SDM Vol.3 §18.2!).
- `single_step_scharf/unscharf(frame)`: TF bit in the SAVED TrapFrame
  (NEVER live — TF in the kernel would trap the kernel itself).
- `hwbreak_setzen/löschen(fach, addr)`: DR0-3 + DR7, start with exec/length 1 only.
- `dr6_lesen()` (check B0-B3 vs. BS/bit 14!), `sync_zu_thread(addrs, maske)`
  (diff against the core buffer, pattern `sync_tls`).
- `DbHook` type + `set_db_hook` (pattern FAULT_HOOK); `#DB` branch in
  `handle_exception()` BEFORE the fault branch (vector 1, EL0 only, otherwise fatal like #NM).

## Contents aarch64 (mirror)

PSTATE.SS, MDSCR_EL1.SS, DBGBVRn/DBGBCRn n=0..3, exec only. Check bit numbers!

## Open points to check

1. Verify all SDM/ARM bit numbers against the manual (from memory!).
2. Is IDT vector 1 really instantiated?
3. Is an IST needed for #DB, or is the thread stack ok?
4. TCB home (`dbg_addr/maske`, migration!) + allocation table + ERR_NOSPACE.
5. aarch64 EC codes + VENTRY hook.
