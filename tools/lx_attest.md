# Z7 offline: what `lx_attest_check.py` proves — and what it does not

As of 2026-09-09. The tool recomputes the SW-PCR measurement chain outside the machine
(no boot, no writes to existing files). Invocations:

```sh
tools/lx_attest_check.py demo               # Formel gegen Goldwerte (kein Boot)
tools/lx_attest_check.py kette bericht.json # PD-Bericht: Kette + Nonce + Modell-Signatur
tools/lx_attest_check.py log build/diag/lauf.log  # Kernel-Log: Konsistenz der messkette-Zeilen
```

## Where the pieces come from

| Piece | Source | Why there |
|---|---|---|
| Genesis (anchor) | `tools/kernel_hash.py <kernel.elf>` | The anchor IS the kernel code hash (`[__text_start, __rodata_end)`) — the same one the manifest binds to via `kernel_hash`. A report over a different anchor is a report about a different kernel. |
| `image_hash` per entry | build artefacts / manifest (`e.sha256`) | What the kernel measures is `SHA-256(ELF bytes)` — the same bytes whose hash is in the manifest. Known hashes means: from THAT build, not from memory. |
| `program_id`/`domain` | manifest entry | The chain binds ID + domain INTO the extend — a report that names a different program does not compute. |
| Nonce | tenant (fresh per request) | Replay protection: the tenant sends a nonce, the PD binds it into the signed report. `nonce_erwartet` is the one sent, `bericht.nonce` the one that came back. |
| Signature (production) | PD key (manifest endowment), Ed25519 | The tenant checks against the manifest public key of the attestation PD. THAT is NOT here — it is tenant side with existing crypto (`caprock-trust`), not a new tool. |
| Signature (model) | `--modell-key` + `signatur` in the JSON | The TEST MAC from `programs/attest` (`TestSchluessel`). With it the PROTOCOL LOGIC can be shown offline (key change breaks, forgery breaks) without claiming Ed25519. |

## What "passed" means — and what it does not

* `kette: ALL PASS` means: the chain computes from the anchor to the head, the head
  is in the report, the count matches the list, the nonce is fresh, the (model)
  signature comes from this key. Nothing more.
* It does NOT mean "the machine is genuine": without a TPM there is no hardware anchor
  (SW-PCR + signed report, no remote trust against a physical attacker).
  The TPM measurement chain (firmware → bootloader → kernel + module) comes from outside
  (`tools/lx_bootentscheidung.md` §2); this tool starts only at the kernel.
* `log` is consistency, not proof: the kernel lines carry 4-byte prefixes each, for
  diagnostic purposes — only `kette` over the PD's JSON report checks completely.
* `verworfen != 0` is FAILURES, not a cosmetic flaw: a chain with a gap proves
  nothing about what was discarded — it only names that something is missing.
