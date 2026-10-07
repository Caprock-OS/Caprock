//! Exception-Vektoren und Trap-Dispatch (aarch64, EL1).
//!
//! Eine 2-KiB-ausgerichtete Vektortabelle mit 16 Einträgen (4 Quellen × 4
//! Typen). Jeder Eintrag sichert den vollen GP-Registerkontext in einen
//! [`TrapFrame`] auf dem Stack, ruft [`handle_exception`] und stellt den Kontext
//! per `eret` wieder her. Assembler ist eine erlaubte `unsafe`-Domäne.

use super::cpu;
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Der zweite Summand der C4-Stackrechnung. **Eine** Definition (`crate::irqtiefe`), hier nur
/// unter dem gewohnten Namen weitergereicht — damit `hal::exception::irq_tiefe()` auf beiden
/// Architekturen existiert und die Aufrufstellen im Kernel arch-neutral bleiben duerfen.
///
/// Bis zum 2026-08-13 gab es diese Funktionen **nur** auf x86, waehrend der arch-neutrale Kernel
/// sie rief: drei `E0425`, der aarch64-Bau kaputt seit dem 2026-08-12.
pub use crate::irqtiefe::{irq_tiefe, irq_tiefe_melden};

/// Auf dem Stack gesicherter Registerkontext einer Exception.
///
/// Das Layout entspricht exakt der Speichersequenz im Vektor-Assembler. Der
/// FP/SIMD-Zustand liegt **nicht** im Frame: er wird **lazy** verwaltet (siehe
/// [`crate::fp`]) — der Trap-Pfad ist integer-only und kostet keine 512 Byte
/// FP-Sicherung pro Trap. FP-Register gehören pro Kern dem aktuellen FP-Owner;
/// erst ein FP-Trap (EC 0x07) aus EL0 löst Save/Restore aus.
#[repr(C, align(16))]
pub struct TrapFrame {
    /// x0..x30 (x30 = Link Register).  Offset 0..248
    pub gpr: [u64; 31],
    /// `ELR_EL1` — Rücksprungadresse (unterbrochener PC).  Offset 248
    pub elr: u64,
    /// `SPSR_EL1` — gesicherter Prozessorstatus.  Offset 256
    pub spsr: u64,
    /// `SP_EL0` — User-Stack-Pointer (nur relevant für EL0-Threads).  Offset 264 -> 272
    pub sp_el0: u64,
}

global_asm!(
    r#"
.macro VENTRY id
.balign 0x80
    sub     sp, sp, #272
    stp     x0,  x1,  [sp, #0]
    stp     x2,  x3,  [sp, #16]
    stp     x4,  x5,  [sp, #32]
    stp     x6,  x7,  [sp, #48]
    stp     x8,  x9,  [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    mrs     x9,  ELR_EL1
    mrs     x10, SPSR_EL1
    stp     x30, x9,  [sp, #240]
    str     x10, [sp, #256]
    mrs     x9,  sp_el0            // User-Stack-Pointer sichern (EL0-Threads)
    str     x9,  [sp, #264]
    mov     x1,  #\id
    b       __trap_dispatch
.endm

.section .text
.balign 0x800
.globl __exception_vectors
__exception_vectors:
    VENTRY 0    // Current EL, SP0:  Sync / IRQ / FIQ / SError
    VENTRY 1
    VENTRY 2
    VENTRY 3
    VENTRY 4    // Current EL, SPx:  Sync / IRQ / FIQ / SError  (Kernel läuft hier)
    VENTRY 5
    VENTRY 6
    VENTRY 7
    VENTRY 8    // Lower EL, aarch64
    VENTRY 9
    VENTRY 10
    VENTRY 11
    VENTRY 12   // Lower EL, aarch32
    VENTRY 13
    VENTRY 14
    VENTRY 15

__trap_dispatch:
    // Integer-only Trap-Pfad: KEIN FP/SIMD-Save (Lazy-FP, siehe crate::fp).
    mov     x0, sp                // x0 = &TrapFrame ; x1 = vector id
    bl      handle_exception
    mov     sp, x0                // x0 = wiederherzustellender Frame (ggf. anderer Thread)

    // GP-Kontext wiederherstellen.
    ldr     x10, [sp, #256]
    msr     SPSR_EL1, x10
    ldr     x9,  [sp, #264]        // User-Stack-Pointer wiederherstellen (EL0)
    msr     sp_el0, x9
    ldp     x30, x9,  [sp, #240]
    msr     ELR_EL1, x9
    ldp     x28, x29, [sp, #224]
    ldp     x26, x27, [sp, #208]
    ldp     x24, x25, [sp, #192]
    ldp     x22, x23, [sp, #176]
    ldp     x20, x21, [sp, #160]
    ldp     x18, x19, [sp, #144]
    ldp     x16, x17, [sp, #128]
    ldp     x14, x15, [sp, #112]
    ldp     x12, x13, [sp, #96]
    ldp     x10, x11, [sp, #80]
    ldp     x8,  x9,  [sp, #64]
    ldp     x6,  x7,  [sp, #48]
    ldp     x4,  x5,  [sp, #32]
    ldp     x2,  x3,  [sp, #16]
    ldp     x0,  x1,  [sp, #0]
    add     sp, sp, #272
    eret
"#
);

/// `VBAR_EL1` auf die Vektortabelle setzen. Pro Kern aufzurufen.
pub fn init() {
    extern "C" {
        static __exception_vectors: u8;
    }
    // SAFETY: Setzen von `VBAR_EL1` auf die statische, 2-KiB-ausgerichtete
    // Vektortabelle des Kernels. Interrupt-/Exception-Init ist eine erlaubte
    // `unsafe`-Domäne.
    unsafe {
        let vbar = core::ptr::addr_of!(__exception_vectors) as u64;
        asm!("msr VBAR_EL1, {}", in(reg) vbar, options(nomem, nostack, preserves_flags));
    }
    cpu::isb();
}

/// True, wenn `kind` einen IRQ-Eintrag bezeichnet (Typ 1 in jeder 4er-Gruppe).
fn is_irq(kind: u64) -> bool {
    kind % 4 == 1
}

/// **Typisierter atomarer Hook-Slot.** Die Trap-Hooks müssen aus dem IRQ-Kontext lock-frei
/// lesbar sein, also liegen sie als `usize` in einem Atomic. Früher stand an jeder der fünf
/// Lesestellen ein eigenes `transmute::<usize, KonkreterHookTyp>` — fünf `unsafe`-Stellen, bei
/// denen ein Vertipper einen Hook als **falschen Funktionstyp** aufgerufen hätte (UB, vom
/// Compiler ungeprüft). Dieser Wrapper bindet den Slot per `PhantomData` an **genau einen**
/// Funktionszeigertyp: `store`/`load` sind typgeprüft, die Verwechslung ist strukturell
/// unmöglich, und die Roh-Konvertierung existiert nur noch **einmal** (hier).
struct AtomicHook<F: Copy> {
    /// Funktionszeiger als `usize`; `0` = nicht gesetzt.
    raw: AtomicUsize,
    _marker: core::marker::PhantomData<fn() -> F>,
}

impl<F: Copy> AtomicHook<F> {
    const fn new() -> Self {
        Self {
            raw: AtomicUsize::new(0),
            _marker: core::marker::PhantomData,
        }
    }

    /// Hook setzen (einmalig beim Boot, vor dem Aktivieren von IRQs).
    fn store(&self, hook: F) {
        const {
            // Nur Funktionszeiger (zeigergroß, kein Fat Pointer) passen in den Slot.
            assert!(core::mem::size_of::<F>() == core::mem::size_of::<usize>());
        }
        // SAFETY: `F` ist per const-Assertion zeigergroß; ein Funktionszeiger hat dieselbe
        // Repräsentation wie `usize` (aarch64). Nur die Roh-Bits werden abgelegt.
        let raw = unsafe { core::mem::transmute_copy::<F, usize>(&hook) };
        self.raw.store(raw, Ordering::Release);
    }

    /// Hook lesen; `None`, solange keiner registriert ist. Lock-frei (IRQ-Kontext).
    fn load(&self) -> Option<F> {
        let raw = self.raw.load(Ordering::Acquire);
        if raw == 0 {
            return None;
        }
        // SAFETY: `raw` wurde ausschließlich von `store` mit einem gültigen Zeiger **genau
        // dieses** Typs `F` geschrieben (der Slot ist über `PhantomData` an `F` gebunden, es
        // gibt keinen anderen Schreibpfad). Größengleichheit ist const-geprüft.
        Some(unsafe { core::mem::transmute_copy::<usize, F>(&raw) })
    }
}

/// Optionaler Reschedule-Hook (vom Scheduler registriert). Wird einmalig beim Boot
/// gesetzt und danach nur gelesen.
static RESCHED_HOOK: AtomicHook<RescheduleHook> = AtomicHook::new();

/// Signatur des Reschedule-Hooks: bekommt den aktuellen TrapFrame, liefert den
/// wiederherzustellenden (ggf. den eines anderen Threads).
pub type RescheduleHook = fn(*mut TrapFrame) -> *mut TrapFrame;

/// Reschedule-Hook registrieren (vor dem Aktivieren von IRQs aufzurufen).
pub fn set_reschedule_hook(hook: RescheduleHook) {
    RESCHED_HOOK.store(hook);
}

/// Optionaler **Geräte-IRQ-Hook** (ext-22, P5): bekommt eine INTID, die weder Timer noch
/// Reschedule-IPI ist. Der Kernel vermerkt sie (pending) + maskiert sie am Distributor und
/// gibt `true` zurück, falls es ein **registrierter** Geräte-IRQ (mit IRQ-Cap) war — dann
/// fährt der Dispatch einen Reschedule (der Drain stellt sie als Notification zu, außerhalb
/// des IRQ-Kontexts). Der Hook selbst nimmt **keinen** Lock (IRQ-Kontext).
static IRQ_HOOK: AtomicHook<IrqHook> = AtomicHook::new();

/// Signatur des Geräte-IRQ-Hooks: INTID -> war es ein registrierter Geräte-IRQ?
pub type IrqHook = fn(u32) -> bool;

/// Geräte-IRQ-Hook registrieren (vor dem Aktivieren von IRQs aufzurufen).
pub fn set_irq_hook(hook: IrqHook) {
    IRQ_HOOK.store(hook);
}

fn device_irq(intid: u32) -> bool {
    match IRQ_HOOK.load() {
        Some(hook) => hook(intid),
        None => false,
    }
}

fn reschedule(frame: *mut TrapFrame) -> *mut TrapFrame {
    match RESCHED_HOOK.load() {
        Some(hook) => hook(frame),
        None => frame,
    }
}

/// Optionaler Syscall-Hook (vom IPC-System registriert).
static SYSCALL_HOOK: AtomicHook<SyscallHook> = AtomicHook::new();

/// Signatur des Syscall-Hooks (Argumente/Rückgabe im TrapFrame).
pub type SyscallHook = fn(*mut TrapFrame) -> *mut TrapFrame;

/// Syscall-Hook registrieren (vor dem Aktivieren von IRQs/Threads aufzurufen).
pub fn set_syscall_hook(hook: SyscallHook) {
    SYSCALL_HOOK.store(hook);
}

fn syscall(frame: *mut TrapFrame) -> *mut TrapFrame {
    match SYSCALL_HOOK.load() {
        Some(hook) => hook(frame),
        None => frame,
    }
}

/// Optionaler Fault-Hook für **synchrone Faults aus EL0** (User-Thread greift auf
/// Kernel-Speicher zu, führt eine privilegierte Instruktion aus o. Ä.). Statt den
/// Kernel anzuhalten, beendet der Hook den fehlerhaften User-Thread und liefert
/// den nächsten lauffähigen Frame zurück.
static FAULT_HOOK: AtomicHook<FaultHook> = AtomicHook::new();

/// Signatur des EL0-Fault-Hooks: aktueller (fehlerhafter) Frame + `ESR`/`FAR` zur
/// Diagnose; liefert den wiederherzustellenden Frame des nächsten Threads.
pub type FaultHook = fn(*mut TrapFrame, u64, u64) -> *mut TrapFrame;

/// EL0-Fault-Hook registrieren (vor dem Aktivieren von IRQs/Threads aufzurufen).
pub fn set_fault_hook(hook: FaultHook) {
    FAULT_HOOK.store(hook);
}

/// Optionaler FP-Trap-Hook (vom Lazy-FP-Subsystem registriert). Behandelt einen
/// FP/SIMD-Zugriffstrap (EC 0x07) aus EL0: Owner-Wechsel der FP-Register.
static FP_HOOK: AtomicHook<FpHook> = AtomicHook::new();

/// Signatur des FP-Trap-Hooks: aktueller Frame -> wiederherzustellender Frame
/// (i. d. R. derselbe; der `eret` wiederholt die getrappte Instruktion).
pub type FpHook = fn(*mut TrapFrame) -> *mut TrapFrame;

/// FP-Trap-Hook registrieren (vor dem Aktivieren von IRQs/Threads aufzurufen).
pub fn set_fp_hook(hook: FpHook) {
    FP_HOOK.store(hook);
}

fn fp_trap(frame: *mut TrapFrame) -> *mut TrapFrame {
    match FP_HOOK.load() {
        Some(hook) => hook(frame),
        // Kein Lazy-FP-Subsystem: FP-Trap ist hier unerwartet -> als Fault behandeln.
        None => frame,
    }
}

/// Register `xidx` eines (gesicherten) TrapFrames lesen. Für den
/// Nachrichtentransfer zwischen Threads (IPC).
pub fn frame_reg(frame: usize, idx: usize) -> u64 {
    // SAFETY: `frame` ist ein gültiger, vom Trap-Pfad angelegter TrapFrame-Zeiger
    // (Kontext-/Trap-Domäne). `idx` < 31 wird vom Aufrufer (ABI) eingehalten.
    unsafe { (*(frame as *const TrapFrame)).gpr[idx] }
}

/// Register `xidx` eines (gesicherten) TrapFrames schreiben (IPC-Transfer).
pub fn frame_set_reg(frame: usize, idx: usize, val: u64) {
    // SAFETY: wie `frame_reg`; schreibender Zugriff auf den gesicherten Kontext.
    unsafe {
        (*(frame as *mut TrapFrame)).gpr[idx] = val;
    }
}

/// `SPSR` des Frames (z. B. um das Exception-Level des Aufrufers zu bestimmen:
/// die Modus-Bits `[3:0]` sind `0b0000` für EL0t, `0b0101` für EL1h).
pub fn frame_spsr(frame: usize) -> u64 {
    // SAFETY: gültiger TrapFrame-Zeiger (Kontext-/Trap-Domäne).
    unsafe { (*(frame as *const TrapFrame)).spsr }
}

/// `true`, wenn der Frame von EL0 stammt (User-Thread).
pub fn frame_from_el0(frame: usize) -> bool {
    frame_spsr(frame) & 0xf == 0
}

// --- Frame-SERIALISIERUNG (Z26/A3, die Nutzlast) ----------------------------------------------
//
// Gegenstück zur x86-Fassung; die Begründung für die feldweise Schreibweise steht dort.

/// Wörter eines serialisierten aarch64-Frames: `x0..x30`, `elr`, `spsr`, `sp_el0`.
pub const FRAME_WOERTER: usize = 34;

/// Davon **übernehmbar** (die Allzweckregister). `elr`/`spsr`/`sp_el0` werden aus dem Sidecar
/// **nie** zurückgeschrieben — `spsr` trägt das Exception-Level, also den Ring.
pub const FRAME_GPR: usize = 31;

/// Architekturkennung im Sidecar-Kopf (`caprock_sched::redirect::ARCH_AARCH64`).
pub const FRAME_ARCH: u64 = 1;

/// Frame-Wort-Index der ABI-Register `x0..x6`. Auf aarch64 ist die Abbildung die Identität —
/// die Tabelle steht trotzdem da, weil der **Handler** nicht wissen soll, auf welcher
/// Architektur er läuft.
pub const FRAME_ABI_WORT: [u64; 7] = [0, 1, 2, 3, 4, 5, 6];

/// Den gesicherten Frame als Wortfolge ablegen (s. x86-Fassung).
pub fn frame_woerter(frame: usize, out: &mut [u64]) -> usize {
    if out.len() < FRAME_WOERTER {
        return 0;
    }
    // SAFETY: wie `frame_reg` -- gültiger, vom Trap-Pfad angelegter TrapFrame-Zeiger.
    let f = unsafe { &*(frame as *const TrapFrame) };
    out[..FRAME_GPR].copy_from_slice(&f.gpr);
    out[31] = f.elr;
    out[32] = f.spsr;
    out[33] = f.sp_el0;
    FRAME_WOERTER
}

/// **Nur die Allzweckregister** aus einer Wortfolge in den Frame übernehmen (s. x86-Fassung).
pub fn frame_gpr_uebernehmen(frame: usize, w: &[u64]) -> usize {
    let n = FRAME_GPR.min(w.len());
    // SAFETY: wie `frame_set_reg` -- schreibender Zugriff auf den gesicherten Kontext.
    let f = unsafe { &mut *(frame as *mut TrapFrame) };
    f.gpr[..n].copy_from_slice(&w[..n]);
    n
}

/// **Ein einzelnes Frame-Wort schreiben** (Z6b) — s. die x86-Fassung fuer die Begruendung, warum
/// hier ein zweites Gatter steht und warum das keine Doppelung ist.
///
/// `spsr` ist auf dieser Architektur das Ringwort: es traegt das **Exception-Level** und die
/// Maskenbits. Anders als auf x86 gibt es damit **kein maskierbares Flagregister** — der Flagteil
/// steckt im selben Wort wie das EL, und ein Wort, das man nur halb schreiben darf, wird hier gar
/// nicht geschrieben.
pub fn frame_wort_setzen(frame: usize, i: usize, v: u64) -> bool {
    // SAFETY: wie `frame_set_reg`.
    let f = unsafe { &mut *(frame as *mut TrapFrame) };
    match i {
        0..=30 => {
            f.gpr[i] = v;
            true
        }
        31 => {
            f.elr = v;
            true
        }
        32 => false, // `spsr` -- Exception-Level + Masken
        33 => {
            f.sp_el0 = v;
            true
        }
        _ => false,
    }
}

/// Einen initialen TrapFrame anlegen, sodass der Trap-Restore-Epilog per `eret`
/// in `entry(arg)` springt. `stack_top` ist der (Kernel-)Stack, auf dem der Frame
/// liegt; bei einem EL0-Thread (`el0 = true`) läuft der Thread auf dem separaten
/// `user_sp` und auf EL0, sonst auf EL1 mit `stack_top`. Gibt den Frame-Zeiger
/// (initialer gespeicherter SP des Threads) zurück.
pub fn init_thread_frame(
    stack_top: usize,
    entry: usize,
    arg: usize,
    el0: bool,
    user_sp: usize,
) -> usize {
    let frame_addr = stack_top - core::mem::size_of::<TrapFrame>();
    // SAFETY: `frame_addr` liegt in einem frisch allozierten, exklusiv besessenen
    // Stack (RW, identity-gemappt). Wir initialisieren genau einen TrapFrame, den
    // der Restore-Epilog konsumiert. Thread-Kontext-Setup ist erlaubte Domäne.
    unsafe {
        let f = frame_addr as *mut TrapFrame;
        (*f).gpr = [0; 31];
        (*f).gpr[0] = arg as u64; // x0 = Argument
        (*f).elr = entry as u64; // Resume-PC
        // SPSR-Modus: EL0t (0b0000) für User-Threads, sonst EL1h (0b0101). DAIF=0.
        (*f).spsr = if el0 { 0x0 } else { 0x5 };
        (*f).sp_el0 = user_sp as u64;
    }
    frame_addr
}

/// Zentraler Trap-Handler (aus dem Assembler gerufen). Rückgabe: der
/// wiederherzustellende TrapFrame (normalerweise `frame`, bei einem
/// Scheduler-Switch der eines anderen Threads).
#[no_mangle]
pub extern "C" fn handle_exception(frame: *mut TrapFrame, kind: u64) -> *mut TrapFrame {
    if is_irq(kind) {
        let intid = super::gic::handle_irq();
        // Timer-Tick oder Cross-Core-Reschedule-IPI -> neu einplanen.
        if intid == Some(crate::timer::intid())
            || intid == Some(super::gic::IPI_RESCHED_INTID)
        {
            return reschedule(frame);
        }
        // Geräte-IRQ (ext-22, P5): an den Kernel-Hook melden (pending + maskieren). War es
        // ein registrierter Geräte-IRQ, fährt der Reschedule den Drain (Notification-Signal
        // außerhalb des IRQ-Kontexts -> Sperrordnung gewahrt).
        if let Some(id) = intid {
            if device_irq(id) {
                return reschedule(frame);
            }
        }
        return frame;
    }

    // Nicht-IRQ: ESR auswerten.
    let esr: u64;
    // SAFETY: read-only Systemregister, keine Seiteneffekte.
    unsafe {
        asm!("mrs {}, ESR_EL1", out(reg) esr, options(nomem, nostack, preserves_flags));
    }
    let ec = (esr >> 26) & 0x3f;
    if ec == 0x15 {
        // SVC (AArch64) -> Syscall. ELR zeigt bereits hinter das `svc`.
        return syscall(frame);
    }
    if ec == 0x07 {
        // Zugriff auf FP/SIMD aus EL0 getrappt (Lazy-FP): Owner-Wechsel. Der Hook
        // lädt den FP-Kontext des Threads und gibt den Frame unverändert zurück;
        // der `eret` führt die getrappte FP-Instruktion dann erneut aus.
        return fp_trap(frame);
    }

    // Echter Fault: diagnostizieren.
    let far: u64;
    // SAFETY: read-only Systemregister.
    unsafe {
        asm!("mrs {}, FAR_EL1", out(reg) far, options(nomem, nostack, preserves_flags));
    }

    // Fault aus EL0 (User-Thread): isolieren statt anhalten. Der registrierte Hook
    // beendet den fehlerhaften Thread und liefert den nächsten lauffähigen Frame.
    // So kann ein User-Thread den Kernel nicht zum Absturz bringen.
    if frame_from_el0(frame as usize) {
        if let Some(hook) = FAULT_HOOK.load() {
            return hook(frame, esr, far);
        }
    }

    // EL1-Fault (Kernel-Bug) oder kein Hook: diagnostizieren + anhalten.
    // SAFETY: `frame` zeigt auf den gültigen, vom Vektor angelegten Stack-Frame.
    let frame = unsafe { &*frame };

    super::console::emit_raw("\n[EXCEPTION] unerwarteter Trap\n");
    super::console::emit_fmt(format_args!(
        "  kind={} ({})\n  ESR={:#018x} (EC={:#04x})\n  ELR={:#018x}\n  FAR={:#018x}\n  SPSR={:#018x}\n",
        kind,
        kind_name(kind),
        esr,
        (esr >> 26) & 0x3f,
        frame.elr,
        far,
        frame.spsr,
    ));
    cpu::halt();
}

fn kind_name(kind: u64) -> &'static str {
    match kind {
        0..=3 => "Current EL SP0",
        4..=7 => "Current EL SPx",
        8..=11 => "Lower EL aarch64",
        _ => "Lower EL aarch32",
    }
}
