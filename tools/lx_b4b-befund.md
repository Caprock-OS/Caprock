# B4b measurement rig: finding 2026-09-10 (virtio-blk sends no MSI-X)

Tool: `tools/lx_b4b-messstand.sh` (builds/renews the archive, boots one variant with a
log excerpt). Logs under `build/diag/lx-b4b-*.{log,auszug,qemu-err}` (gitignored).

## Measurement rig (about 3 min each, 4 runs + setup)

| Run | Variant | used.idx | Queue vector | IRTE | Faults | IRQ? |
|---|---|---|---|---|---|---|
| Setup | TCG+intremap, old archive | – | – | – | – | False start: the manifest did not bind to the kernel (archive 08-28 vs. kernel 09-09) |
| L1 | TCG, intremap=on, B4 off | 1 | `0xffff` (none) | – | – | no (expected: queue never programmed) |
| M1 | TCG, intremap=on, measuring mode | 1 | `0x0000` | present, SVT/SID, row `addr=0xfee00008` unmasked, `msix-ctrl=0x8004` | empty | no (`weckrufe=0`, only self-IPI) |
| M2 | TCG, intremap=on, **eim=on**, measuring mode | 1 | `0x0000` | identical | empty | no (character-identical to M1) |
| Rollback | TCG, intremap=on, B4 off | 1 | `0xffff` | – | – | Sanity: `drv`/`blkdev` ALL PASS again |

Proved throughout: `irtevgb: ALL PASS`, KOMPAT self-IPI `angekommen=true zugestellt=true`
(vector 0x64), `apic: x2APIC (MSR-Pfad)`.

New measuring mode in the demo driver (`programs/hardware/virtio-blk`, default OFF): `B4_MESSEN`
programs the queue onto row 0 like the product path, waits with the measurer's deadline
(`wait_frist`, 200 ticks, `ERR_TIMEOUT`, no poll fallback). Without it all
surrounding runs talk over a never-programmed queue (`0xffff`).

## Cause (status: hypothesis, no proof)

- **KVM exonerated:** M1 under TCG reproduces the KVM finding exactly (programmed
  queue vector + `used.idx=1` + no IRQ + no faults). "KVM/split-irqchip" was not it.
- **The QEMU flag alone is not enough:** M2 with `eim=on` is character-identical to M1.
- **Remaining suspicion (format, kernel side):** the kernel runs **x2APIC**
  (`intc.rs:init_cpu` switches it on; log: MSR path), but the IOMMU side is
  **xAPIC-format**: `vtd.rs:976-979` leaves out `IRTA.EIME` (bit 11), `irte.rs:34-37/96-113`
  encodes the destination as an 8-bit ID in bits 47:40 (rejection >255). x2APIC CPU against
  an EIME=0 IRTE: QEMU then silently does not deliver a remapped message (no fault).
  The next step is the EIME run (patch below + `eim=on`), not `B4_WARTEN`.
- Run without intremap not driven (budget): `irte_vergabe_moeglich()==false` →
  `msi_grant()=None` → the driver rightfully polls. Contributes nothing to the delivery question.

## PATCH TEXT (HAL ownership, NOT applied -- check against the VT-d manual)

QEMU side: `-device intel-iommu,caching-mode=on,intremap=on,eim=on` with
`-machine q35,kernel-irqchip=split`. `eim` only makes sense with `intremap`; on its own it heals
nothing (M2). Precondition for an EIME=1 IRTE to be deliverable to an
x2APIC LAPIC at all.

```patch
--- a/crates/caprock-hal/src/x86_64/vtd.rs   (ir_enable, Kontext Zeilen 969-979)
+++ b/crates/caprock-hal/src/x86_64/vtd.rs
@@
     let Some(table) = alloc() else { return false };
     // Groessenfeld in den unteren Bits, Adresse in 63:12. EIME (Bit 11) bleibt aus: es gilt nur
-    // im x2APIC-Modus, und den meldet diese Plattform nicht zwingend -- ein gesetztes EIME ohne
-    // x2APIC waere ein reserviertes Bit.
-    write64(REG_IRTA, (table & !0xfff) | IRTA_SIZE_FIELD);
+    // im x2APIC-Modus, und den meldet diese Plattform nicht zwingend -- ein gesetztes EIME ohne
+    // x2APIC waere ein reserviertes Bit. B4b-Messstand (M1/M2, TCG): die CPU laeuft x2APIC
+    // (apic-Zeile: MSR-Pfad), die IRTE bleibt xAPIC-formatig -- programmierter Queue-Vektor,
+    // used.idx=1, keine Faults, kein IRQ. EIME deshalb genau dann setzen, wenn der Kern
+    // wirklich x2APIC faehrt:
+    //   let irta = (table & !0xfff) | IRTA_SIZE_FIELD
+    //       | if super::intc::x2apic_active() { 1 << 11 } else { 0 };
+    //   write64(REG_IRTA, irta);
+    // BEGLEITEND, gleiche Stufe: irte_build muss das Ziel dann im x2APIC-Format kodieren
+    // (32-Bit-Destination statt 8-Bit in 47:40; die Abweisung apic_id > 0xFF entfaellt dort)
+    // -- Bitlage vor dem Anwenden gegen das VT-d-Manual (IRTA.EIME / IRTE-Format) pruefen.
+    // Ohne diese zweite Haelfte waere EIME=1 mit 8-Bit-Ziel ein neues stilles Fehlformat.
+    write64(REG_IRTA, (table & !0xfff) | IRTA_SIZE_FIELD);
```
