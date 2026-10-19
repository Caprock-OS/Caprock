# Slot-0 collision: decision paper (2026-09-10)

## Finding (verified, not believed)

Slot convention of the loader ABI (`kernel/src/loader.rs`, documented at `endow_from_manifest`):
`0` = loader cap, `1` = notification, `2` = endpoint, `3–8` = device/service caps.

- **Manifest side:** with the `CAP_LOADER` bit, `endow_from_manifest` fixes the loader cap
  on slot 0 (`loader.rs:668–674`; the task statement named 660–666 — drifted by ~8 lines,
  same block: `install_loader_cap(0, …)` → `out[0]`).
- **`init` side:** `programs/trusted/init/src/main.rs:158–159` ALWAYS delegates the
  badged notification copy to target slot 0 (`&[(NTFN_CHILD_SLOT, 0)]`, doc: "slot 0
  of the new PD — the loader ABI convention L2"). `LOADER_SLOT = 0` (l. 95).
- **SYS_LOAD side:** `load_by_index` (`loader.rs:3036–3052`) first puts the caller's offer
  into `slots` and checks every manifest cap against it: the same slot twice → `collision`,
  no start, delegated copies are taken back (fail-closed, named in the log).
- **LXPD side:** `lxpddrv_laden` (`loader.rs:1696–1701`) combines manifest caps +
  `caller_endow` into one buffer: the same slot twice → `LxpdAbsage::EndowKollision`
  (with clean-up of freshly minted caps, l. 1698–1700), too many → `EndowZuViel`.
- **Dispatch side:** `sammle_endow` (`crates/caprock-microkit/src/lib.rs:1788–1845`,
  one place for `LOAD` and `LOAD_IMAGE`) rejects duplicate target slots already in the offer.

**When does it really collide? Only on double occupancy of slot 0:** the manifest grants
`CAP_LOADER` (slot 0) AND the caller delegates to slot 0. Each side alone is
collision-free — a single cap on slot 0 occupies it exactly once.

**Do driver manifests request `CAP_LOADER`? As a rule no** — a driver needs
MMIO/DMA/IRQ/ntfn/ep, no loading authority. The only counter-proof in the tree is
deliberately built the other way round: `tools/lxpd-e2e.sh --mit-lader` gives the *service* PD
itself the `loader` bit (`CAPS="loader,ntfn,ep,shared"`, l. 89) — and expects exactly the
rejection (init reports index 6 as failed, bit 7 in the root badge, no `lxpdimg`).
That proves the barrier with lines instead of words.

**What does the LXPD service delegate where? Nothing.** `AnstossKontext`
(`programs/lxpd-runtime/src/lib.rs:242–255`) only passes through — the service does not guess
slots ("The service invents no hand-over, it only passes it through", l. 910–911).
`lxpdrv` (`programs/lxpd-runtime/src/bin/lxpdrv.rs:196–203`) kicks off with
`deleg_liste = 0, deleg_anzahl = 0` → `caller_endow` is empty → **no collision,
whatever the driver manifest has in slot 0** (one source, one cap, no opponent).

**Path walk-through with real slots (driver without slot 0 + empty delegation):**
manifest `{ntfn→1, ep→2, mmio→3/4/5/6}` + offer `{}` → `alle = [1,2,3,4,5,6]`, no slot
twice → start. Counter-proof (service WITH the `loader` bit via init): manifest `{loader→0,…}`
+ offer `{(child→0)}` → slot 0 twice → rejection, the child does not start, init sets the
bit — exactly the `--mit-lader` picture. The rejection is thus the
named outcome on both paths, not a silent one.

Side finding (only read, not touched — foreign ownership): `tools/lx_driver_manifest.py:40`
still carries the FNV typo `...2325` instead of the standard `...25c5` (see AGENTS.md message 14).

## Option 1 — the rejection suffices (fail-closed, no rework)

`EndowKollision`/`collision` remain the named outcome. Reason: the collision
requires a double occupancy, and no real path produces it — init children without
`CAP_LOADER`, LXPD service without delegation. Whoever wants to delegate some day already reads off the
manifest today whether slot 0 is free. Cost: zero lines.

## Option 2 — redirection (manifest slot 0 or delegation yields)

E.g. put the loader cap on a fallback slot, bend the delegation or forbid `CAP_LOADER`
for driver entries. Cost: a silent choice ("which cap lands on 0?") —
exactly the form that A-5.4/`service_id` abolished — plus a new convention in three
places (manifest, dispatch, service) that can age apart again.

## Recommendation: option 1

The E2E finding ("rejection sufficient") withstands re-examination: verified at
`endow_from_manifest` (slot-0 source), `init` (slot-0 offer), `load_by_index`/
`lxpddrv_laden` (collision check) and `lxpdrv`/`AnstossKontext` (empty offer).
The `--mit-lader` expectation in the E2E script proves the barrier positively. No need to act;
re-evaluate at the first real delegation by a run-time service.
