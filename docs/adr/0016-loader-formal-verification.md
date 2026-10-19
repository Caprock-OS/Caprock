# ADR 0016 — Formal functional verification of the loader (Verus, phase 2)

Status: **accepted** · Date: 2026-06-27 · Phase 2 of functional verification.
Reference: ADR 0011 (binary loader), ADR 0014 (TrustedSAS certificates), ADR 0015 (verification approach),
`Verification/loader/`, `docs/verification.md`.

## Context

Phase 1 (capability system) established the methodology: abstract, faithful model + Verus proof,
real code unchanged, multi-layered (audit+fuzzer+Kani+Verus). Phase 2 verifies the **loader** —
specifically the **security logic** `verify_image`/`verify_trusted_cert` (ADR 0014) + the load state
machine. The **parsers** (cert/archive/elf) are already proved panic-/OOB-free with **Kani**
(`docs/verification.md`) — they form the foundation; phase 2 builds the **functional** layer on top.

## Analysis

`verify_image(prog)`: UserLand/HardwareLand → `true` (hardware-isolated); TrustedSAS →
`verify_trusted_cert` = conjunction of checks (algorithm Ed25519 + signature length 64, `key_id` in the
read-only key DB non-revoked + self-consistent, signature over the message, `binary_hash`/
`manifest_hash` binding, `program_id`/`version` identity, anti-downgrade, `unsafe_status==ALL_PASS`).
`load_image` calls `verify_image` **before** any resource allocation → atomic (no partial load state).

## Comparison of variants

| Variant | Description | Assessment |
|---|---|---|
| V1 model the crypto too | Ed25519/SHA-256 in Verus | unnecessary + huge; crypto = established library (TCB). Rejected. |
| **V2 abstract the crypto (chosen)** | `sig_valid`/`*_hash_ok` as **facts** protected by the signature; prove the gate/state-machine **logic** | captures exactly the loader's peculiarity (decision logic + atomicity); the fidelity of the crypto is carried by the libraries + the round-trip test (ext-28) |
| V3 real-code annotation | annotate `loader.rs` directly | changes the architecture (prohibited); deferred |

## Decision

**V2** — abstract the crypto/hash facts, formally verify the **gate + load state machine logic**.
Properties to prove: **soundness** (no unverified TrustedSAS loads),
**program integrity/identity binding**, **revocation** (+ completeness), **domain gating**
(UserLand/HardwareLand need no cert), **atomicity** (exactly one load outcome, no inconsistent
state), **determinism** (read-only key DB → reproducible). Abstract model; real code
unchanged; multi-layered (the parsers' memory safety is delivered by Kani).

## Consequences

- **Positive:** the TrustedSAS load security (ADR 0014) is formally proved, not only checked by self-test/
  fuzzer. Clear layering: Kani (parser memory safety) + Verus (gate/state-machine logic).
- **Limits/TCB:** the crypto primitives (ed25519-dalek/sha2) + the model↔code fidelity are the trust
  base (backed by the host round trip + the run-time self-tests/`trust_audit`). The
  capability **endowment** of the loaded PD (domain policy at load time) is a later stage of expansion,
  builds on phase 1 (domain policy already proved).
