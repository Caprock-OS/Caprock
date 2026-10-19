# S0 A-header contract: Linux symbol → Caprock schema

As of: 2026-09-09. Source: `docs/linux-kompatibilitaet-caprock.md`
section 2 (A/B edge, l. 34–182), section 5 (Caprock gap, l. 331–366),
M1/M2 (l. 622–641); primitives checked against
`crates/caprock-dma/src/lib.rs`, `crates/caprock-wait/src/lib.rs`,
`crates/caprock-region/src/{lib,page,heap}.rs`, `crates/caprock-sync/src/lib.rs`.

## How to read it

- **Class A** is mapped onto Caprock by templates during translation.
  A row without a template is a **translation error**, not an empty stub.
- **Class B** is translated from the real Linux source tree and deliberately gets
  **no** template (`fehlt:bewusst`).
- **Kind A-rein** = pure substitution rule, nothing is left behind in the binary.
  **Kind A-Zustand** = the rule generates code with run-time state (the risk
  of the layer).

## Status values

| Status | Means |
|---|---|
| `vorhanden:formell benannt` | Schema exists and is named (rule or PD crate) |
| `vorhanden:noch ohne Frist` | Schema exists, the blocking form waits for the PARK deadline (A2 remainder) |
| `fehlt:Kernel` | Not built; needs Caprock-side work (kernel or PD crate) |
| `fehlt:bewusst` | Deliberately no template (class B: translated, not substituted) |

## Contract table (machine-readable: `lx_symbols.py` parses it)

The table cells are data that `tools/lx_symbols.py` reads (class letter, status values such as
`vorhanden:…`); they stay in German on purpose, so this translation does not blind the parser.

| Linux-Symbol | Klasse | Caprock-Schema | Art | Status |
|---|---|---|---|---|
| kmalloc | A | caprock-region Heap (Slab-Klassen, heap.rs) | A-Zustand | vorhanden:formell benannt |
| kfree | A | Kmaloc::kfree (Vorrat/Recycling; Heap-dealloc ist unsafe und bleibt drin) | A-Zustand | vorhanden:formell benannt |
| ksize | A | caprock-region kmalloc::ksize (Klassengroesse, truesize) | A-Zustand | vorhanden:formell benannt |
| krealloc | A | caprock-region Kmaloc::krealloc (Umzug ohne Kopie — kopieren obliegt dem Shim) | A-Zustand | vorhanden:formell benannt |
| GFP_KERNEL, GFP_ATOMIC | A | caprock-region Gfp (ATOMIC erreicht den Wachstums-Pfad gar nicht: Modul-Vorrat) | A-Zustand | vorhanden:formell benannt |
| alloc_pages | A | caprock-region page: order_zu_len + MemMap, Vergabe via Heap-Large-Pfad | A-Zustand | vorhanden:formell benannt |
| free_pages | A | Region-Release (Heap-Large-shrink) | A-Zustand | vorhanden:formell benannt |
| get_page | A | PageDesc::get_page (saettigend, page.rs) | A-Zustand | vorhanden:formell benannt |
| put_page | A | PageDesc::put_page (Unterlaufschutz) | A-Zustand | vorhanden:formell benannt |
| page_address | A | MemMap::pfn_zu_addr (reine Arithmetik) | A-rein | vorhanden:formell benannt |
| mutex_lock | A | caprock-wait Mutex (Park-Vertrag, null Syscalls unbestritten) | A-Zustand | vorhanden:formell benannt |
| spin_lock | A | caprock-wait Spinlock + Preempt-Gatter je Thread (E3: nur Kernwechsel derselben PD) | A-rein | vorhanden:formell benannt |
| spin_lock_irqsave | A | caprock-sync irq_save_disable + SpinLock; PD-seitig threaded-IRQ-Modell (Mutex) | A-rein | vorhanden:formell benannt |
| atomic_* | A | core::sync::atomic (Sprachmittel, kein Caprock-Schema noetig) | A-rein | vorhanden:formell benannt |
| msleep | A | timeout::msleep ueber TimeoutPark + Clock (SYS_PARK_TIMEOUT, ABI 29) | A-Zustand | vorhanden:formell benannt |
| jiffies | A | caprock-wait jiffies() ueber Clock | A-rein | vorhanden:formell benannt |
| msecs_to_jiffies | A | caprock-wait msecs_to_jiffies (saettigend, stutzt ab) | A-rein | vorhanden:formell benannt |
| timer_setup | A | TimerWheel::after | A-Zustand | vorhanden:formell benannt |
| mod_timer | A | TimerWheel::after (erneutes Eintragen) | A-Zustand | vorhanden:formell benannt |
| del_timer | A | TimerWheel::absagen (Austragen ohne Wecken, idempotent) | A-Zustand | vorhanden:formell benannt |
| queue_work | A | Workqueue::queue | A-Zustand | vorhanden:formell benannt |
| queue_delayed_work | A | Komposition msleep + Workqueue::queue (kein eigener Typ noetig) | A-Zustand | vorhanden:formell benannt |
| flush_workqueue | A | Workqueue::flush (nur Leere-Nachweis, kein blockierendes Flush) | A-Zustand | vorhanden:noch ohne Frist |
| wait_event | A | caprock-wait wait_event (pruefen, einreihen, parken) | A-Zustand | vorhanden:formell benannt |
| wait_event_timeout | A | timeout::wait_event_timeout (absolute Frist, Austragen bei Ablauf) | A-Zustand | vorhanden:formell benannt |
| complete | A | Completion::complete (Zaehler, kein Flag) | A-Zustand | vorhanden:formell benannt |
| wait_for_completion | A | Completion::warten | A-Zustand | vorhanden:formell benannt |
| wait_for_completion_timeout | A | Completion::warten_timeout (Zaehler bleibt bei Ablauf stehen) | A-Zustand | vorhanden:formell benannt |
| ioremap | A | Kernel/HAL-MMU-Mapping (gemessen, §5) | A-Zustand | vorhanden:formell benannt |
| readl | A | Load auf gemapptes BAR + Barriere | A-rein | vorhanden:formell benannt |
| writel | A | Store auf gemapptes BAR + Barriere | A-rein | vorhanden:formell benannt |
| ioread32 | A | wie readl (32-Bit-Form) | A-rein | vorhanden:formell benannt |
| iowrite32 | A | wie writel (32-Bit-Form) | A-rein | vorhanden:formell benannt |
| wmb | A | Fence-Intrinsic | A-rein | vorhanden:formell benannt |
| rmb | A | Fence-Intrinsic | A-rein | vorhanden:formell benannt |
| dma_alloc_coherent | A | DmaPool::alloc (Name abweichend, Faehigkeit gleich) | A-Zustand | vorhanden:formell benannt |
| dma_map_single | A | map_single (Pool-Arithmetik, None ausserhalb des Pools) | A-rein | vorhanden:formell benannt |
| dma_unmap_single | A | unmap_single (No-op, kohärent auf x86) | A-rein | vorhanden:formell benannt |
| dma_map_sg | A | map_sg (nie verschmelzend: Eingabezahl oder 0) | A-rein | vorhanden:formell benannt |
| dma_sync_single_for_cpu | A | sync_fuer_cpu (No-op, kohärent) | A-rein | vorhanden:formell benannt |
| dma_set_mask | A | GeraeteGrenzen::maske + pruefe_grenzen | A-rein | vorhanden:formell benannt |
| request_irq | A | IRQ-Cap + bind_irq (B1-B3 gemessen; B4-Warten und Multi-Vektor offen) | A-Zustand | vorhanden:formell benannt |
| request_threaded_irq | A | Threaded-IRQ-Modell (Completion + WAIT, Warteweg) | A-Zustand | vorhanden:formell benannt |
| pci_enable_device | B | aus Linux-Quellen uebersetzt (drivers/base, PCI) | — | fehlt:bewusst |
| pci_request_regions | B | aus Linux-Quellen uebersetzt | — | fehlt:bewusst |
| dev_err | A | caprock-log dev_err/dev_warn/dev_info-Senke (LogRing: no_std, nie blockierend, voll verwirft Ältestes + Zähler) | A-Zustand | vorhanden:formell benannt |
| printk | A | caprock-log printk-Senke + kern_stufe-Abbildung (KERN_ERR 0–3/WARNING 4–5/INFO 6–7 → Err/Warn/Info, Rest None) | A-Zustand | vorhanden:formell benannt |

## Honest gaps (cross-cutting, not per symbol)

As of now: the PARK deadline (SYS_PARK_TIMEOUT, ABI 29, dispatch in microkit), the preemption gate
(spinlock + preempt in caprock-wait), kmalloc/GFP/ksize/krealloc (caprock-region kmalloc),
del_timer (TimerWheel::absagen) and delayed_work (msleep+queue composition) have been present and
host-tested since this step. What remains:

1. **Multi-vector:** one vector per device means single queue; MSI-X with one
   vector per queue is open (§5, B edge) — additionally needs a variable Cspace.
2. **Variable Cspace:** `NCAPS = 16`, hard; for more caps per PD a variable
   Cspace is needed (TODO0 K1c). Affects `request_irq` (cap per vector)
   and every driver PD with a large cap demand.
3. **Logging sink stands, wiring open:** `dev_err`/`printk` map onto
   `caprock-log` (ring, filter, `kern_stufe`); what is missing is the reader side in the
   class layer or router (who drains the ring and where to), no longer the sink.
4. **Blocking `flush_workqueue`:** `Workqueue::flush` only establishes emptiness;
   a flush that waits for running jobs can be built with completion, but is not
   spelled out as a form.
5. **MMIO barriers as a HAL matter:** readl/writel semantics (volatile + fence) live
   in the arch-specific HAL (B-owned) and are only named here as a template rule,
   not as a checked primitive.

## Measurement

- `python3 tools/lx_symbols.py` — rate from this table (M1 approximation).
- `python3 tools/lx_symbols.py --nm <file>` — check `nm -u` output (or a
  one-symbol-per-line file) against the table, list the unknown ones.
- `tools/lx_messen.sh` — M1/M2 state on one line, exit 0.
