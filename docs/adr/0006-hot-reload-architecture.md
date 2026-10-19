# ADR 0006 — Hot-reload architecture

**Status:** implemented (phase 7) · **Date:** 2026-06-23

> **State (phase 7):** hot reload is implemented and verified in QEMU — a
> server PD is replaced in the running system (quiesce: withdraw receiver +
> take away recv cap → swap: start v2 + delegate recv cap onto the *same* endpoint
> → resume), without a kernel restart, transparent to the client (same
> send cap, same endpoint). Primitives: `EndpointTable::retire_receiver`,
> `PdTable::clear_cap`. **Still open:** state checkpoint for stateful
> components (step 2 below), generic reload service, clean-up of the old
> PD (TCB/stack). See `docs/phase-reports/phase-7.md`.

## Motivation

Everything except the kernel and the Microkit runtime must be stoppable,
replaceable and reloadable **without a kernel restart**: drivers, network stack, file systems, services,
userland. A kernel reboot for this is never permissible.

## Basic idea: capabilities as stable identity, component as exchangeable content

A component (e.g. a driver) is a **protection domain** (Microkit PD)
that is connected to the outside world exclusively via capabilities: endpoints
(services it offers/uses), notifications (IRQs), device and
memory capabilities. The **capability graph is the stable interface** —
the concrete code behind it is exchangeable.

## Approaches analysed

### A) Kill the process + restart, clients reconnect
- **−** Connections break; clients must have reconnect logic; state is
  lost; not transparent. Rejected as the standard.

### B) Endpoint rebind behind a stable cap identity (chosen)
A service's **endpoint** is an independent kernel object with a stable
capability. On reload:

1. **Quiesce:** new requests at the endpoint are buffered/blocked (the endpoint
   remains, the server is merely decoupled).
2. **Drain/checkpoint:** the old PD finishes running requests; optionally exports
   state into a **memory cap** (handed to the successor).
3. **Swap:** a new PD is loaded, receives the same device/memory/endpoint caps
   (re-bind) and the checkpoint.
4. **Resume:** the endpoint is bound to the new PD again; buffered requests
   continue. Clients notice nothing (same cap, same endpoint).

- **+** Transparent for clients (stable cap/endpoint identity).
- **+** Cleanly capability-modelled: reload = `revoke` caps from the old PD and `delegate` them to the
  new PD; control lies with a **reload manager** holding the necessary
  authority (itself a userland service with special caps).
- **+** State hand-over via memory cap is zero-copy in the SAS (ADR 0002/0004).
- **−** Services must support a **quiesce/checkpoint protocol** so as not to lose
  state (stateless drivers are trivial; stateful ones need
  cooperation).

### C) Versioned modules with live patching in the same PD (Theseus style)
- **+** Fine-grained, no PD switch.
- **−** Requires deep toolchain/loader integration; conceivable later as an optimisation,
  too complex now. Postponed.

## Decision

**Approach B.** Hot reload via stable endpoint/cap identity: quiesce →
drain/checkpoint → swap (cap revoke/delegate) → resume. The kernel supplies the
primitives (endpoint quiesce/rebind, cap revoke/delegate, notification redirection);
a **reload manager in userland** orchestrates the policy. The kernel and the
Microkit runtime themselves are **not** hot-reloadable (they are the TCB).

## Security implications

- The reload manager is a privileged component (defined cap set) —
  strictly observe least privilege; it may only re-hang the caps of the component
  to be replaced.
- **Revocation correctness** is security-critical: after the swap no
  "dangling" caps of the old PD may remain on devices/memory (CDT revoke, ADR 0003).
- A compromised driver can (without MMU isolation, ADR 0002) cause damage within
  *its* caps, but hot reload permits quick replacement as a
  reaction.

## Performance implications

The quiesce window should be small; buffered requests cause short
latency spikes during the swap. Zero-copy checkpoint via memory cap keeps the
state transfer cheap. No kernel reboot → availability is preserved.
