//! In-Kernel-Fuzzer (ADR 0013) — optionales Verifikationsmodul hinter dem Feature `kernel-fuzz`.
//!
//! Enthaelt die vier aktiven Testfall-Generatoren samt ihren Statics, Konstanten und
//! Treiberschritten:
//!   * `fuzz`       — generativer Kernel-Fuzzer (Cap/Thread/VSpace-Op-Sequenzen, Phase 1 + SMP).
//!   * `ipcfuzz`    — IPC-State-Machine-Fuzzer (nebenlaeufige Aktoren + KILL/Reload/MCS).
//!   * `hwfuzz`     — Domaenen-/HW-/Management-Cap-Churn gegen die Policy.
//!   * `loaderfuzz` — fehlerhafte ELF-Varianten durch den Binary-Loader-Parser.
//!
//! Dieses Modul wird NUR mit `--features kernel-fuzz` einkompiliert; der Release-Kernel enthaelt
//! keinerlei Fuzzer-Code (ein Stub-Modul in `threads/mod.rs` stellt dann dieselbe API als No-Op
//! bereit). Die **Audits** (`domain_audit`/`cap_audit_cdt`/`vspace_audit`/`dma_audit`/
//! `loader_audit`/`ipc_audit`) sind KEIN Teil dieses Moduls — sie bleiben immer im Kernel und
//! werden hier nur aufgerufen. Als Kindmodul von `threads` erreicht es ueber `use super::*` den
//! gesamten Selbsttest-Harness (auch private Items), ohne Kernel-APIs zu oeffnen.
#![allow(clippy::too_many_arguments)]

use super::*;
use caprock_hal::println;
// Diese drei nutzt nur der Fuzzer-Code -> hier explizit importiert (aus threads/mod.rs gezogen).
use caprock_cap::CapPtr;
use caprock_loader::{Program, DOMAIN_TRUSTED, DOMAIN_USERLAND};

// Einmalige Spawn-Flags (ersetzen die frueheren Idle-Loop-Locals `fuzz_spawned`/`ipcfuzz_spawned`).
static FUZZ_SPAWNED: AtomicBool = AtomicBool::new(false);
static IPCFUZZ_SPAWNED: AtomicBool = AtomicBool::new(false);

// --- Generische Fuzzer-Helfer (nur von den Fuzzern benutzt; aus threads.rs hierher gezogen). ---

/// Deterministischer PRNG (xorshift64). Fester Seed -> jeder Fehlschlag reproduzierbar.
fn frand(s: &mut u64) -> u64 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *s = x;
    x
}

/// Zufällige Rechte für copy/mint (Kind erbt ⊆ Eltern via `intersect`).
fn frand_rights(s: &mut u64) -> Rights {
    match frand(s) % 4 {
        0 => Rights::READ,
        1 => Rights::WRITE,
        2 => Rights::RWX,
        _ => Rights::EXEC,
    }
}

/// Index eines zufälligen belegten Pool-Eintrags (oder `None`, falls leer).
fn pick_live<T: Copy>(pool: &[Option<T>], s: &mut u64) -> Option<usize> {
    let n = pool.iter().filter(|x| x.is_some()).count();
    if n == 0 {
        return None;
    }
    let mut k = (frand(s) % n as u64) as usize;
    for (i, x) in pool.iter().enumerate() {
        if x.is_some() {
            if k == 0 {
                return Some(i);
            }
            k -= 1;
        }
    }
    None
}

/// Index eines freien Pool-Eintrags.
fn free_slot<T>(pool: &[Option<T>]) -> Option<usize> {
    pool.iter().position(|x| x.is_none())
}

/// Je Idle-Tick vom Harness aufgerufen: fuehrt den faelligen Fuzzer-Schritt aus, gegate auf die
/// Vorgaenger-Tests (im Harness-Modul). Verhalten identisch zur frueheren Inline-Logik der Idle-Loop.
pub(super) fn drive() {
    // Loader-Fuzzer (ext-26 L5): einmalig, nach loadstop (L4) fertig.
    if !LOADERFUZZ_DONE.load(Ordering::Acquire) && super::LOADSTOP_DONE.load(Ordering::Acquire) {
        LOADERFUZZ_OK.store(run_loaderfuzz(), Ordering::Release);
        LOADERFUZZ_DONE.store(true, Ordering::Release);
    }

    // TrustedSAS-Zertifikats-Fuzzer (ext-28): einmalig, direkt nach dem Loader-Fuzzer.
    if !CERTFUZZ_DONE.load(Ordering::Acquire) && LOADERFUZZ_DONE.load(Ordering::Acquire) {
        CERTFUZZ_OK.store(run_certfuzz(), Ordering::Release);
        CERTFUZZ_DONE.store(true, Ordering::Release);
    }

    // Domaenen/HW-Fuzzer (ext-22): nach dem letzten ext-27-Dienst (cross); eine Epoche je Tick.
    if !HWFUZZ_DONE.load(Ordering::Acquire) && super::CROSS_DONE.load(Ordering::Acquire) {
        match HWFUZZ_STEP.load(Ordering::Acquire) {
            0 => {
                // Fixtures (eine PD je Domaene, ohne Threads) + Baseline schnappen.
                if let Some(tpd) = system::create_pd_in_domain(Domain::TrustedSas) {
                    HWFUZZ_TPD.store(tpd, Ordering::Relaxed);
                    if let Some((hpd, _, _)) = system::create_hardware_backend(tpd, 9) {
                        HWFUZZ_HPD.store(hpd, Ordering::Relaxed);
                    }
                    if let Some(upd) = system::create_pd_in_domain(Domain::UserLand) {
                        HWFUZZ_UPD.store(upd, Ordering::Relaxed);
                    }
                    HWFUZZ_BASE_OBJ.store(system::cap_used_objects(), Ordering::Relaxed);
                    HWFUZZ_BASE_FREE.store(system::total_free(), Ordering::Relaxed);
                    HWFUZZ_STEP.store(1, Ordering::Release);
                }
            }
            _ => {
                let e = HWFUZZ_EPOCH.load(Ordering::Acquire);
                if HWFUZZ_FAIL.load(Ordering::Acquire) == 0 && e < HWFUZZ_EPOCHS {
                    let code = hwfuzz_epoch(
                        HWFUZZ_TPD.load(Ordering::Relaxed),
                        HWFUZZ_HPD.load(Ordering::Relaxed),
                        HWFUZZ_UPD.load(Ordering::Relaxed),
                        HWFUZZ_BASE_OBJ.load(Ordering::Relaxed),
                        HWFUZZ_BASE_FREE.load(Ordering::Relaxed),
                    );
                    if code != 0 {
                        HWFUZZ_FAIL.store(code, Ordering::Release);
                    }
                    HWFUZZ_EPOCH.store(e + 1, Ordering::Release);
                } else {
                    HWFUZZ_OK.store(
                        HWFUZZ_FAIL.load(Ordering::Acquire) == 0
                            && e >= HWFUZZ_EPOCHS
                            && system::domain_audit() == 0,
                        Ordering::Release,
                    );
                    HWFUZZ_DONE.store(true, Ordering::Release);
                }
            }
        }
    }

    // Generativer Fuzzer (Bereich H): EINMALIG den Treiber (core 0, prio 2) spawnen, sobald MCS +
    // stale/strand + hwfuzz durch sind.
    if !FUZZ_SPAWNED.load(Ordering::Acquire)
        && super::MCS_DONE.load(Ordering::Acquire)
        && super::STALE_DONE.load(Ordering::Acquire)
        && super::STRAND_DONE.load(Ordering::Acquire)
        && HWFUZZ_DONE.load(Ordering::Acquire)
    {
        if system::spawn_on_core(0, fuzz_driver as *const () as usize, 0, 2).is_some() {
            FUZZ_SPAWNED.store(true, Ordering::Release);
        }
    }

    // IPC-State-Machine-Fuzzer: EINMALIG den Controller (core 0, prio 3) spawnen, sobald der
    // generative Fuzzer durch ist.
    if !IPCFUZZ_SPAWNED.load(Ordering::Acquire) && FUZZ_DONE.load(Ordering::Acquire) {
        if system::spawn_on_core(0, ipcfuzz_controller as *const () as usize, 0, 3).is_some() {
            IPCFUZZ_SPAWNED.store(true, Ordering::Release);
        }
    }
}

/// Haben alle vier Fuzzer bestanden? (Beitrag zu `all_done()`.)
pub(super) fn all_passed() -> bool {
    LOADERFUZZ_DONE.load(Ordering::Acquire)
        && LOADERFUZZ_OK.load(Ordering::Acquire)
        && CERTFUZZ_DONE.load(Ordering::Acquire)
        && CERTFUZZ_OK.load(Ordering::Acquire)
        && HWFUZZ_DONE.load(Ordering::Acquire)
        && HWFUZZ_OK.load(Ordering::Acquire)
        && FUZZ_DONE.load(Ordering::Acquire)
        && FUZZ_OK.load(Ordering::Acquire)
        && IPCFUZZ_DONE.load(Ordering::Acquire)
        && IPCFUZZ_OK.load(Ordering::Acquire)
}

/// Ketten-Gate: der ext-27-T0-Test (aggru) laeuft nach dem Loader-Fuzzer. (Der Stub im Release-
/// Build gibt stattdessen den Vorgaenger `pred` durch.)
pub(super) fn loaderfuzz_gate(_pred_loadstop: bool) -> bool {
    LOADERFUZZ_DONE.load(Ordering::Acquire)
}

/// Ketten-Gate: der caplk-Test laeuft nach fuzz+ipcfuzz. (Der Stub gibt `pred` durch.)
pub(super) fn fuzzers_gate(_pred_cross: bool) -> bool {
    FUZZ_DONE.load(Ordering::Acquire) && IPCFUZZ_DONE.load(Ordering::Acquire)
}

// DBG-pending-Bools (im Release-Build vom Stub als `true` geliefert).
pub(super) fn dbg_fuzz() -> bool {
    FUZZ_DONE.load(Ordering::Acquire) && FUZZ_OK.load(Ordering::Acquire)
}
pub(super) fn dbg_ipcfuzz() -> bool {
    IPCFUZZ_DONE.load(Ordering::Acquire) && IPCFUZZ_OK.load(Ordering::Acquire)
}
pub(super) fn dbg_hwfuzz() -> bool {
    HWFUZZ_DONE.load(Ordering::Acquire) && HWFUZZ_OK.load(Ordering::Acquire)
}
pub(super) fn dbg_loaderfuzz() -> bool {
    LOADERFUZZ_DONE.load(Ordering::Acquire) && LOADERFUZZ_OK.load(Ordering::Acquire)
}

/// Druckt die vier Fuzzer-Reportzeilen (von `report()` im Harness aufgerufen).
pub(super) fn report() {
    // Generativer Fuzzer (Bereich H): zufaellige Op-Sequenzen, Oracle nach jeder Epoche.
    let fops = FUZZ_OPS.load(Ordering::Relaxed);
    let feo = FUZZ_EPOCHS_OK.load(Ordering::Relaxed);
    let ffail = FUZZ_FAIL.load(Ordering::Relaxed);
    let fsmp = FUZZ_SMP_OK.load(Ordering::Acquire);
    let fsmpops = FUZZ_SMP_OPS.load(Ordering::Relaxed);
    let fuzz = FUZZ_DONE.load(Ordering::Acquire) && FUZZ_OK.load(Ordering::Acquire);
    println!("fuzz    : Phase1 {feo}/{FUZZ_EPOCHS} Epochen, {fops} gepruefte Ops, Oracle-Bruch-Code={ffail} (0=keiner); Phase2 {fsmpops} SMP-Churn-Iter (8 Kerne) ok={fsmp}; gesamt ~{} Ops", fops + fsmpops);
    println!(
        "fuzz    : {} (generativ: zufaellige Cap/MEM/VSpace/Thread/SchedCtx-Sequenzen, Baseline-Oracle, SMP-Kontention)",
        if fuzz { "ALL PASS" } else { "FAILURES" }
    );

    // IPC-State-Machine-Fuzzer (Bereich H, Teil 2): nebenlaeufige Aktoren + Oracle.
    let iops = IPCF_OPS.load(Ordering::Relaxed);
    let ikills = IPCF_KILLS.load(Ordering::Relaxed);
    let ieo = IPCF_EPOCHS_OK.load(Ordering::Relaxed);
    let ianom = IPCF_ANOMALY.load(Ordering::Relaxed);
    let itd = IPCF_TEARDOWN_OK.load(Ordering::Acquire);
    let ipcfuzz = IPCFUZZ_DONE.load(Ordering::Acquire) && IPCFUZZ_OK.load(Ordering::Acquire);
    println!("ipcfuzz : {ieo}/{IPCF_EPOCHS} Epochen, {iops} IPC-Ops, {ikills} KILLs (8 Kerne); Oracle-Anomalie={ianom} (0=keine); Teardown-Baseline={itd}");
    println!(
        "ipcfuzz : {} (nebenlaeufige Endpoint-/Notification-Zustandsmaschinen: KILL/EXIT/Reload/MCS/Cap-Ops waehrend IPC, Queue-Konsistenz-Oracle)",
        if ipcfuzz { "ALL PASS" } else { "FAILURES" }
    );

    // Binary-Loader L5 (ext-26): Loader-Fuzzer + loader_audit.
    let loaderfuzz = LOADERFUZZ_DONE.load(Ordering::Acquire) && LOADERFUZZ_OK.load(Ordering::Acquire);
    println!(
        "loaderfuzz: {} (8 fehlerhafte ELF-Varianten durch load_image -> alle bei parse abgelehnt, kein Crash/OOB (Parser #![forbid(unsafe_code)]), Baseline unveraendert, loader_audit==0)",
        if loaderfuzz { "ALL PASS" } else { "FAILURES" }
    );

    // TrustedSAS-Zertifikats-Fuzzer (ext-28, ADR 0014): mutierte/zufaellige Zertifikate + Binding.
    let certfuzz = CERTFUZZ_DONE.load(Ordering::Acquire) && CERTFUZZ_OK.load(Ordering::Acquire);
    println!(
        "certfuzz: {} ({} Zertifikatsvarianten durch das verify_image-Gate (Mutation/Truncation/Zufallsmuell + Identitaets-/Binary-Transplantation) -> ALLE abgelehnt, echtes Cert akzeptiert, kein Crash/OOB (no-alloc Krypto), trust_audit+loader_audit==0, Baseline unveraendert)",
        if certfuzz { "ALL PASS" } else { "FAILURES" },
        CERTFUZZ_CHECKED.load(Ordering::Relaxed)
    );

    // Domaenen/HW-Fuzzer (ext-22, P6).
    let hwf_fail = HWFUZZ_FAIL.load(Ordering::Acquire);
    let hwfuzz = HWFUZZ_DONE.load(Ordering::Acquire) && HWFUZZ_OK.load(Ordering::Acquire);
    println!("hwfuzz  : {} Epochen HW-/Management-Cap-Churn (MMIO/IRQ/PdControl/DMA + ext-24 Richtung/Kohaerenz/Kontext-Attach/SG) gegen Policy + CDT/VSpace/DMA-Oracle + Baseline; Anomalie-Code={hwf_fail} (0=keine)", HWFUZZ_EPOCH.load(Ordering::Acquire));
    println!(
        "hwfuzz  : {} (HW-Caps nur HardwareLand, PdControl nur TrustedSas, MMIO/IRQ-Delete fasst RAM-Allokator nicht an, DMA-alloc<->free balanciert, Audits stets 0)",
        if hwfuzz { "ALL PASS" } else { "FAILURES" }
    );
}

// =========================================================================================
// Verschobene Fuzzer-Items (unveraendert aus threads.rs uebernommen, ADR 0013).
// __ITEMS_BELOW__

// Binary-Loader L5 (ext-26): Loader-Fuzzer -- fehlerhafte ELFs durch load_image -> alle abgelehnt,
// kein Crash, Balance, loader_audit==0.
static LOADERFUZZ_DONE: AtomicBool = AtomicBool::new(false);
static LOADERFUZZ_OK: AtomicBool = AtomicBool::new(false);

// Domänen/HW-Fuzzer (ext-22, P6): churnt über viele Epochen Hardware-/Management-Caps gegen
// die Domänen-Policy + CDT/VSpace-Oracles + Ressourcen-Baseline. Kernpunkte: HW-Caps (MMIO/
// IRQ) NUR in HardwareLand, PdControl NUR in TrustedSas installierbar (Negativfälle abgelehnt);
// nach jeder Epoche domain_audit/cap_audit_cdt/vspace_audit == 0 und Baseline wiederhergestellt
// — insbesondere ändert das Löschen von MMIO/IRQ-Caps `total_free` NICHT (Geräte != RAM).
const HWFUZZ_EPOCHS: u32 = 32;
static HWFUZZ_TPD: AtomicUsize = AtomicUsize::new(usize::MAX); // TrustedSas-Fixture
static HWFUZZ_HPD: AtomicUsize = AtomicUsize::new(usize::MAX); // HardwareLand-Fixture
static HWFUZZ_UPD: AtomicUsize = AtomicUsize::new(usize::MAX); // UserLand-Fixture
static HWFUZZ_BASE_OBJ: AtomicUsize = AtomicUsize::new(0); // Baseline belegter Cap-Objekte
static HWFUZZ_BASE_FREE: AtomicU64 = AtomicU64::new(0); // Baseline freier RAM (total_free)
static HWFUZZ_LCG: AtomicU64 = AtomicU64::new(0x1234_5678_9abc_def1); // deterministischer PRNG
static HWFUZZ_EPOCH: AtomicU32 = AtomicU32::new(0);
static HWFUZZ_FAIL: AtomicU32 = AtomicU32::new(0); // 0=ok, sonst Anomalie-Code
static HWFUZZ_STEP: AtomicU32 = AtomicU32::new(0);
static HWFUZZ_DONE: AtomicBool = AtomicBool::new(false);
static HWFUZZ_OK: AtomicBool = AtomicBool::new(false);

/// Deterministischer LCG-Schritt (kein `Math::random`; reproduzierbar über den Seed).
fn hwfuzz_rand() -> u64 {
    let mut x = HWFUZZ_LCG.load(Ordering::Relaxed);
    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    HWFUZZ_LCG.store(x, Ordering::Relaxed);
    x >> 16
}

/// Eine Fuzzer-Epoche: HW-/Management-Caps mit variierenden Parametern prägen, gegen die
/// Domänen-Policy installieren (richtige Domäne klappt, falsche wird abgelehnt), den CDT mit
/// einer Kopie stressen, mitten in der Epoche die Oracles prüfen, dann alles abräumen und die
/// Ressourcen-Baseline verifizieren. Gibt `0` zurück oder einen Anomalie-Code.
fn hwfuzz_epoch(tpd: usize, hpd: usize, upd: usize, base_obj: usize, base_free: u64) -> u32 {
    let phys = 0x0900_0000u64 + (hwfuzz_rand() % 64) * 0x1000; // variierende MMIO-Seite (GiB 0)
    let intid = 40 + (hwfuzz_rand() % 64) as u32; // variierende SPI-INTID
    let len = 0x1000u64 * (1 + hwfuzz_rand() % 4);
    // 1. MMIO-Cap: NUR HardwareLand.
    let mmio = match system::install_mmio_cap(phys, len, Rights::READ) {
        Ok(c) => c,
        Err(_) => return 1,
    };
    if !system::install_pd_cap(hpd, 5, mmio) {
        return 2; // HardwareLand: muss klappen
    }
    if system::install_pd_cap(upd, 5, mmio) || system::install_pd_cap(tpd, 5, mmio) {
        return 3; // UserLand/TrustedSas: HW-Cap muss abgelehnt werden
    }
    // 2. IRQ-Cap: NUR HardwareLand.
    let irq = match system::install_irq_cap(intid, Rights::READ) {
        Ok(c) => c,
        Err(_) => return 5,
    };
    if !system::install_pd_cap(hpd, 6, irq) {
        return 6;
    }
    if system::install_pd_cap(upd, 6, irq) {
        return 7; // UserLand: IRQ-Cap muss abgelehnt werden
    }
    // 3. PdControl: NUR TrustedSas.
    let pdc = match system::install_pd_control_cap(upd, Rights::WRITE) {
        Ok(c) => c,
        Err(_) => return 8,
    };
    if !system::install_pd_cap(tpd, 5, pdc) {
        return 9; // TrustedSas: muss klappen (Slot 5 frei, da MMIO dort abgelehnt wurde)
    }
    if system::install_pd_cap(hpd, 7, pdc) || system::install_pd_cap(upd, 7, pdc) {
        return 10; // HardwareLand/UserLand: PdControl muss abgelehnt werden
    }
    // 4. CDT mit einer HW-Cap-Kopie stressen.
    let mmio2 = match system::cap_copy(mmio, Rights::READ) {
        Ok(c) => c,
        Err(_) => return 11,
    };
    if !system::install_pd_cap(hpd, 8, mmio2) {
        return 12;
    }
    // 4b. DMA-Cap (ext-23/24): echtes kernel-ausgeschnittenes RAM, NUR HardwareLand. Anders als
    // MMIO/IRQ reduziert die Allokation `total_free` — die Revoke (delete_leaf -> free_region)
    // stellt es wieder her, sodass die Epoche balanciert bleibt (Codes 70+). ext-24: variierte
    // Richtung/Kohärenz (install_dma_cap_ex).
    let dlen = 0x1000u64 * (1 + hwfuzz_rand() % 4);
    let dregion = match system::alloc_dma_region(dlen) {
        Some(r) => r,
        None => return 70, // GiB-1-Erschöpfung wäre eine Anomalie (Epoche allokiert+gibt frei)
    };
    let dir = match hwfuzz_rand() % 3 {
        0 => DmaDir::DeviceRead,
        1 => DmaDir::DeviceWrite,
        _ => DmaDir::Bidirectional,
    };
    let coh = if hwfuzz_rand() & 1 == 0 {
        DmaCoherence::Coherent
    } else {
        DmaCoherence::NonCoherent
    };
    let dma = match system::install_dma_cap_ex(dregion.base, dregion.len, dir, coh, Rights::RW) {
        Ok(c) => c,
        Err(_) => {
            system::free_unattached_dma_region(dregion.base, dregion.len);
            return 71;
        }
    };
    if !system::install_pd_cap(hpd, 9, dma) {
        return 72; // HardwareLand: muss klappen
    }
    if system::install_pd_cap(upd, 9, dma) || system::install_pd_cap(tpd, 9, dma) {
        return 73; // UserLand/TrustedSas: DMA-Cap (Hardware) muss abgelehnt werden
    }
    // 4c. ext-24: DmaContext-Churn — Region an eine Fuzz-StreamID anhängen, SG validieren, lösen
    // (balanciert: attach alloziert Stage-1-Frames, detach gibt sie wieder frei).
    let fsid = 0x60u32 + (hwfuzz_rand() % 8) as u32;
    if let Some(h) = system::dma_attach(fsid, dma) {
        if system::testsupport::dma_ctx_region_count(fsid) != 1 {
            return 75;
        }
        let sg = [system::DmaSgEntry { handle: h, offset: 0, len: dregion.len }];
        let bad = [system::DmaSgEntry { handle: h, offset: dregion.len, len: 0x1000 }];
        if !system::dma_sg_validate(fsid, &sg) || system::dma_sg_validate(fsid, &bad) {
            return 76; // SG-Validierung inkonsistent
        }
        system::dma_detach(fsid, h);
    } else {
        return 77; // attach muss klappen (SMMU aktiv)
    }
    // 5. Mid-Epoch-Oracles.
    if system::domain_audit() != 0 {
        return 30;
    }
    if system::cap_audit_cdt() != 0 {
        return 20;
    }
    if system::vspace_audit() != 0 {
        return 40;
    }
    if system::dma_audit() != 0 {
        return 74;
    }
    // 6. Abräumen -> Baseline. (Kind mmio2 VOR dem Elter mmio löschen.)
    system::clear_pd_cap(hpd, 5);
    system::clear_pd_cap(hpd, 6);
    system::clear_pd_cap(hpd, 8);
    system::clear_pd_cap(hpd, 9);
    system::clear_pd_cap(tpd, 5);
    let _ = system::cap_delete(mmio2);
    let _ = system::cap_delete(mmio);
    let _ = system::cap_delete(irq);
    let _ = system::cap_delete(pdc);
    // DMA-Cap löschen -> delete_leaf gibt die RAM-Region via free_region zurück (balanciert
    // mit der alloc_dma_region oben; total_free kehrt zur Baseline zurück).
    let _ = system::cap_delete(dma);
    // 7. Baseline: keine Objekt-Leaks UND `total_free` unveraendert. MMIO/IRQ-Delete fasst den
    // RAM-Allokator NICHT an (Geraet != RAM); DMA-Delete gibt seine Region zurueck -> beides
    // zusammen balanciert auf die Baseline (alloc_dma_region <-> free_region je Epoche).
    if system::cap_used_objects() != base_obj {
        return 50;
    }
    if system::total_free() != base_free {
        return 60;
    }
    0
}

/// **Loader-Fuzzer** (ext-26, L5): fehlerhafte ELF-Images durch den vollen Ladepfad
/// (`load_image`) schicken. Jede Variante korrumpiert genau EIN vom Parser validiertes Feld eines
/// ansonsten gueltigen Minimal-ELF -> MUSS mit `Err` abgelehnt werden (kein Crash, der Parser ist
/// `#![forbid(unsafe_code)]`), die Ressourcen-Baseline darf sich NICHT aendern (Ablehnung bei
/// `parse`, vor jeder Allokation), und `loader_audit() == 0`. Synchron, IRQ-maskiert.
fn run_loaderfuzz() -> bool {
    hal::cpu::local_irq_disable();
    let base = system::total_free();
    let mut buf = [0u8; 128];
    // Ein minimales GUELTIGES ELF64 (ET_EXEC AArch64, 1 PT_LOAD, filesz=0) bauen.
    let build = |b: &mut [u8; 128]| {
        *b = [0u8; 128];
        b[0..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
        b[4] = 2; // ELFCLASS64
        b[5] = 1; // ELFDATA2LSB
        b[6] = 1; // EI_VERSION
        b[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        b[18..20].copy_from_slice(&0xB7u16.to_le_bytes()); // EM_AARCH64
        b[24..32].copy_from_slice(&0x6000_0000u64.to_le_bytes()); // e_entry
        b[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        b[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        b[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        b[64..68].copy_from_slice(&1u32.to_le_bytes()); // p_type=PT_LOAD
        b[68..72].copy_from_slice(&5u32.to_le_bytes()); // p_flags=R+X
        b[72..80].copy_from_slice(&120u64.to_le_bytes()); // p_offset
        b[80..88].copy_from_slice(&0x6000_0000u64.to_le_bytes()); // p_vaddr
        b[104..112].copy_from_slice(&0x1000u64.to_le_bytes()); // p_memsz (filesz=0)
    };
    // Jede Korruption macht den Parser ablehnen (validiertes Feld).
    let corrupts: [fn(&mut [u8; 128]); 8] = [
        |b| b[0] = 0,                                          // Bad-Magic
        |b| b[4] = 1,                                          // ELFCLASS32
        |b| b[5] = 2,                                          // Big-Endian
        |b| b[18] = 0x3E,                                      // EM_X86_64
        |b| b[16] = 3,                                         // ET_DYN
        |b| b[54..56].copy_from_slice(&99u16.to_le_bytes()),   // falsche phentsize
        |b| b[56..58].copy_from_slice(&9999u16.to_le_bytes()), // phnum out-of-bounds
        |b| b[96..104].copy_from_slice(&0xFFFFu64.to_le_bytes()), // p_filesz > Puffer
    ];
    let mut all_rejected = true;
    for c in corrupts.iter() {
        build(&mut buf);
        c(&mut buf);
        let prog = Program::new(99, b"fuzz", 1, DOMAIN_USERLAND, [0u8; 32], &buf, &[], &[]);
        if loader::load_image(&prog, &[]).is_ok() {
            all_rejected = false; // ein fehlerhaftes ELF wurde geladen -> FAIL
        }
    }
    let after = system::total_free();
    let audit = system::loader_audit();
    hal::cpu::local_irq_enable();
    all_rejected && after == base && audit == 0
}

// TrustedSAS-Zertifikats-Fuzzer (ext-28, ADR 0014): mutierte/zufaellige Zertifikate durch das
// verify_image-Gate -> ALLE abgelehnt, kein Crash/OOB (Parser + Krypto #![forbid(unsafe_code)] /
// no-alloc); das ECHTE Zertifikat als Positiv-Kontrolle akzeptiert; Identitaets-/Binary-Bindung
// adversarial (gueltiges Cert auf falsche program_id/version/ELF -> abgelehnt); danach trust_audit /
// loader_audit == 0 und total_free unveraendert (Verifikation alloziert nichts).
static CERTFUZZ_DONE: AtomicBool = AtomicBool::new(false);
static CERTFUZZ_OK: AtomicBool = AtomicBool::new(false);
static CERTFUZZ_CHECKED: AtomicU64 = AtomicU64::new(0); // Anzahl gepruefter Zertifikatsvarianten

const CERTFUZZ_ITERS: u32 = 600;
const CERTFUZZ_BUF: usize = 512;
const CERTFUZZ_SEED: u64 = 0x7CE2_F022_2026_0627; // fester Seed -> reproduzierbar

/// Eine TrustedSAS-Program-Huelle mit dem gegebenen Zertifikat bauen (gleiche Identitaet/Binary wie
/// das Seed-Programm `tx`, nur `cert` variiert) — fuer den verify-only-Pfad (laedt nichts).
fn certfuzz_prog<'a>(tx: &Program<'a>, cert: &'a [u8]) -> Program<'a> {
    Program::new(
        tx.program_id, b"cfz", tx.version, DOMAIN_TRUSTED, [0u8; 32], tx.elf, tx.manifest, cert,
    )
}

fn run_certfuzz() -> bool {
    hal::cpu::local_irq_disable();
    let base = system::total_free();
    let ok = (|| {
        let archive = loader::read_archive()?;
        // Das erste echte TrustedSAS-Zertifikat als Seed (trusted-x = signiertes svc-demo).
        let tx = archive
            .iter()
            .find(|p| p.domain == DOMAIN_TRUSTED && !p.cert.is_empty())?;
        let cert = tx.cert;
        if cert.is_empty() || cert.len() > CERTFUZZ_BUF {
            return Some(false);
        }
        // Positiv-Kontrolle: das ECHTE Zertifikat MUSS akzeptiert werden (Gate ist nicht always-false).
        if !loader::verify_only(&tx) {
            return Some(false);
        }
        let mut s = CERTFUZZ_SEED;
        let mut buf = [0u8; CERTFUZZ_BUF];
        let mut checked = 0u64;
        for _ in 0..CERTFUZZ_ITERS {
            buf[..cert.len()].copy_from_slice(cert);
            let mut len = cert.len();
            match frand(&mut s) % 12 {
                0 => {
                    let i = (frand(&mut s) as usize) % len;
                    buf[i] ^= (frand(&mut s) as u8) | 1; // ein beliebiges Byte SICHER veraendern
                }
                1 => len = (frand(&mut s) as usize) % len, // truncate (stets kuerzer)
                2 => buf[0..4].fill(0),                     // Magic zerstoeren
                3 => buf[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes()), // bogus signature_algorithm_id
                4 => buf[26..30].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes()), // program_id im Cert
                5 => buf[30..34].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()), // version im Cert
                6 => buf[34..66].iter_mut().for_each(|b| *b ^= 0xFF), // binary_hash
                7 => buf[98..114].iter_mut().for_each(|b| *b ^= 0xFF), // key_id (unbekannt)
                8 => buf[114..118].copy_from_slice(&0b011u32.to_le_bytes()), // unsafe_status != ALL_PASS
                9 => {
                    // Signatur (letzte 64 B) verfaelschen.
                    let span = 64.min(len);
                    let i = len - 1 - ((frand(&mut s) as usize) % span);
                    buf[i] ^= (frand(&mut s) as u8) | 1;
                }
                10 => {
                    // komplett zufaelliger Muell variabler Laenge.
                    len = (frand(&mut s) as usize) % CERTFUZZ_BUF;
                    for b in buf[..len].iter_mut() {
                        *b = frand(&mut s) as u8;
                    }
                }
                _ => len = (frand(&mut s) as usize) % 4, // 0..3 Bytes (zu kurz)
            }
            if loader::verify_only(&certfuzz_prog(&tx, &buf[..len])) {
                return Some(false); // EINE Variante akzeptiert -> FAIL
            }
            checked += 1;
        }
        // Identitaets-/Binary-Bindung adversarial: das ECHTE Zertifikat auf eine FALSCHE Identitaet
        // bzw. ein FREMDES Binary heften -> muss abgelehnt werden (Cert ist nicht transplantierbar).
        let fake_elf: &[u8] = b"not-the-certified-binary";
        let wrong_id = Program::new(tx.program_id ^ 0x5A5A, b"cfz", tx.version, DOMAIN_TRUSTED, [0u8; 32], tx.elf, tx.manifest, cert);
        let wrong_ver = Program::new(tx.program_id, b"cfz", tx.version.wrapping_add(1), DOMAIN_TRUSTED, [0u8; 32], tx.elf, tx.manifest, cert);
        let wrong_elf = Program::new(tx.program_id, b"cfz", tx.version, DOMAIN_TRUSTED, [0u8; 32], fake_elf, tx.manifest, cert);
        if loader::verify_only(&wrong_id) || loader::verify_only(&wrong_ver) || loader::verify_only(&wrong_elf) {
            return Some(false);
        }
        checked += 3;
        CERTFUZZ_CHECKED.store(checked, Ordering::Relaxed);
        Some(true)
    })()
    .unwrap_or(false);
    let after = system::total_free();
    let taudit = loader::trust_audit();
    let laudit = system::loader_audit();
    hal::cpu::local_irq_enable();
    ok && after == base && taudit == 0 && laudit == 0
}

// --- Generativer Kernel-Fuzzer (Audit Bereich H) ---
// DETERMINISTISCH (fester Seed -> jeder Fehlschlag reproduzierbar). Ein dedizierter
// Treiber-Thread auf core 0 fährt zufällige Operationssequenzen über Capabilities/
// Speicher/VSpaces/Threads/SchedContexts. Phase 1: pro Epoche N zufällige Ops, die
// verfolgte Objekte erzeugen/mutieren, dann VOLLSTÄNDIGER Teardown + ORACLE — alle
// Ressourcenstände (MEM/TCB/VSpace/kstack + Cap-Slots/Cap-Objekte) müssen exakt zur
// Baseline zurückkehren (sonst Leak/Zombie/CDT-Verletzung). Phase 2: balancierte
// Cap/MEM-Churn parallel auf allen Kernen (SMP-Lock-Kontention), danach Baseline-Check.
const FUZZ_EPOCHS: u32 = 12;
const FUZZ_OPS_PER_EPOCH: u32 = 50;
const FUZZ_CAP_POOL: usize = 24;
const FUZZ_THREAD_POOL: usize = 10;
const FUZZ_MAP_POOL: usize = 8;
const FUZZ_SEED: u64 = 0x5E14_1A4E_2026_0624; // fester Seed
const FUZZ_SMP_ITERS: u32 = 800; // Treiber-Churn-Iterationen während der SMP-Phase
static FUZZ_DONE: AtomicBool = AtomicBool::new(false);
static FUZZ_OK: AtomicBool = AtomicBool::new(false);
static FUZZ_OPS: AtomicU64 = AtomicU64::new(0); // ausgeführte Phase-1-Operationen
static FUZZ_EPOCHS_OK: AtomicU32 = AtomicU32::new(0); // bestandene Epochen
static FUZZ_FAIL: AtomicU32 = AtomicU32::new(0); // 0=ok, sonst Invarianten-Code (1..6)
// Phase 2 (SMP): Noise-Worker auf cores 1..NUM_CORES + Treiber-Churn auf core 0.
static FUZZ_SMP_GO: AtomicBool = AtomicBool::new(false);
static FUZZ_SMP_STOP: AtomicBool = AtomicBool::new(false);
static FUZZ_SMP_ACK: AtomicU32 = AtomicU32::new(0);
static FUZZ_SMP_OK: AtomicBool = AtomicBool::new(false);
static FUZZ_SMP_OPS: AtomicU64 = AtomicU64::new(0); // balancierte Churn-Iterationen (alle Kerne)

/// Ressourcen-Snapshot fürs Oracle: (MEM frei, TCBs core0, freie VSpaces, freie
/// kstack-Pool-Slots, belegte Cap-Slots, belegte Cap-Objekte).
fn fuzz_snapshot() -> (u64, usize, usize, usize, usize, usize) {
    (
        system::total_free(),
        system::used_tcbs(0),
        system::free_vspaces(),
        system::user_kstack_free_count(),
        system::cap_used_slots(),
        system::cap_used_objects(),
    )
}

/// Oracle: Baseline-Wiederherstellung prüfen. Gibt einen Invarianten-Code (1..6) des
/// ERSTEN Bruchs zurück, sonst `None`. 1=MEM,2=TCB,3=VSpace,4=kstack,5=Slots,6=Objekte.
fn fuzz_check(base: &(u64, usize, usize, usize, usize, usize)) -> Option<u32> {
    let now = fuzz_snapshot();
    if now.0 != base.0 {
        return Some(1);
    }
    if now.1 != base.1 {
        return Some(2);
    }
    if now.2 != base.2 {
        return Some(3);
    }
    if now.3 != base.3 {
        return Some(4);
    }
    if now.4 != base.4 {
        return Some(5);
    }
    if now.5 != base.5 {
        return Some(6);
    }
    // CDT-/Refcount-Property nach dem Teardown (Code 20 + audit-Code, s. audit_cdt).
    let cdt = system::cap_audit_cdt();
    if cdt != 0 {
        return Some(20 + cdt);
    }
    None
}

/// Eine zufällige Operation ausführen (alle nicht-blockierend, kernelintern). Die
/// Pools modellieren die vom Fuzzer verfolgten Objekte. Fehlschläge (volle Pools,
/// stale Caps) werden tolerant übersprungen.
#[allow(clippy::too_many_arguments)]
fn fuzz_step(
    s: &mut u64,
    caps: &mut [Option<CapPtr>; FUZZ_CAP_POOL],
    threads: &mut [Option<(u64, bool)>; FUZZ_THREAD_POOL],
    maps: &mut [Option<(u64, u64)>; FUZZ_MAP_POOL],
) {
    match frand(s) % 12 {
        0 => {
            // Memory-Cap installieren (Wurzel; Frame wird beim Löschen freigegeben).
            if let Some(i) = free_slot(caps) {
                if let Some(c) = system::alloc(4096, 4096) {
                    if let Ok(p) = system::cap_install(c) {
                        caps[i] = Some(p);
                    }
                    // (cap_install verbraucht c; bei Err — cspace voll — geht der Frame
                    // verloren, tritt bei beschränkten Pools nicht auf.)
                }
            }
        }
        1 | 2 => {
            // copy / mint: Kind ableiten (Rechte ⊆ Eltern).
            if let (Some(src), Some(dst)) = (pick_live(caps, s), free_slot(caps)) {
                let p = caps[src].unwrap();
                let r = frand_rights(s);
                let res = if frand(s) & 1 == 0 {
                    system::cap_copy(p, r)
                } else {
                    system::cap_mint(p, r, frand(s))
                };
                if let Ok(np) = res {
                    caps[dst] = Some(np);
                }
            }
        }
        3 => {
            // move: Cap an einen neuen Slot verschieben (altes Handle wird ungültig).
            if let Some(i) = pick_live(caps, s) {
                if let Ok(np) = system::cap_move(caps[i].unwrap()) {
                    caps[i] = Some(np);
                }
            }
        }
        4 => {
            // delete (blatt-only): bei Kindern Fehler -> übersprungen.
            if let Some(i) = pick_live(caps, s) {
                if system::cap_delete(caps[i].unwrap()).is_ok() {
                    caps[i] = None;
                }
            }
        }
        5 => {
            // revoke: löscht Nachfahren von caps[i]. Danach können ANDERE Einträge
            // stale sein -> einsammeln.
            if let Some(i) = pick_live(caps, s) {
                let _ = system::cap_revoke(caps[i].unwrap());
                for c in caps.iter_mut() {
                    if let Some(p) = *c {
                        if system::cap_inspect(p).is_none() {
                            *c = None;
                        }
                    }
                }
            }
        }
        6 => {
            // plain-Thread spawnen (prio 0 -> läuft nie, reiner Ressourcen-Halter).
            if let Some(i) = free_slot(threads) {
                if let Some(t) = system::spawn_on_core(0, balanced_worker as *const () as usize, 0, 0)
                {
                    threads[i] = Some((t.to_raw(), false));
                }
            }
        }
        7 => {
            // isolierten EL0-Thread spawnen (eigene VSpace + ASID + kstack).
            if let Some(i) = free_slot(threads) {
                if let Some((t, _)) =
                    system::spawn_isolated(churn_dummy as *const () as usize, 0, 0)
                {
                    threads[i] = Some((t.to_raw(), true));
                }
            }
        }
        8 => {
            // Thread zerstören (vollständiger Teardown). Zugehörige Mappings verwerfen.
            if let Some(i) = pick_live(threads, s) {
                let (raw, iso) = threads[i].unwrap();
                let tid = ThreadId::from_raw(raw);
                if iso {
                    system::destroy_isolated(tid);
                } else {
                    system::kill_local(tid);
                }
                while system::reap() > 0 {}
                for m in maps.iter_mut() {
                    if matches!(*m, Some((mt, _)) if mt == raw) {
                        *m = None;
                    }
                }
                threads[i] = None;
            }
        }
        9 => {
            // Frame in eine isolierte VSpace mappen (separater Frame + eigene Cap).
            let iso = pick_live(threads, s).filter(|&i| threads[i].map_or(false, |(_, iso)| iso));
            if let (Some(ti), Some(ci), Some(mi)) = (iso, free_slot(caps), free_slot(maps)) {
                let (raw, _) = threads[ti].unwrap();
                if let Some(c) = system::alloc(4096, 4096) {
                    let base = c.base();
                    if let Ok(p) = system::cap_install(c) {
                        caps[ci] = Some(p);
                        let perm = (frand(s) % 3) as u8; // 0=Ro,1=Rw,2=Rx
                        if system::map_into_thread(ThreadId::from_raw(raw), base, 4096, perm) {
                            maps[mi] = Some((raw, base));
                        }
                    }
                }
            }
        }
        10 => {
            // Eine Mapping wieder entfernen (per-Seite-Unmap-Pfad).
            if let Some(i) = pick_live(maps, s) {
                let (raw, base) = maps[i].unwrap();
                system::unmap_into_thread(ThreadId::from_raw(raw), base, 4096);
                maps[i] = None;
            }
        }
        _ => {
            // SchedContext-Cap prägen + an einen Thread binden.
            if let (Some(ti), Some(ci)) = (pick_live(threads, s), free_slot(caps)) {
                let (raw, _) = threads[ti].unwrap();
                let budget = (frand(s) % 8) as u32;
                let period = 1 + (frand(s) % 64) as u32;
                if let Ok(sc) = system::install_sched_context_cap(budget, period, Rights::WRITE) {
                    caps[ci] = Some(sc);
                    system::bind_sched_context(sc, 0, ThreadId::from_raw(raw));
                }
            }
        }
    }
}

/// Alles abbauen, was eine Epoche erzeugt hat — Reihenfolge: erst Threads (deren
/// VSpace-Teardown entfernt Mappings), dann Caps (gibt Frames frei). So kein
/// dangling Mapping auf einen bereits freigegebenen Frame.
fn fuzz_teardown(
    caps: &mut [Option<CapPtr>; FUZZ_CAP_POOL],
    threads: &mut [Option<(u64, bool)>; FUZZ_THREAD_POOL],
    maps: &mut [Option<(u64, u64)>; FUZZ_MAP_POOL],
) {
    for t in threads.iter_mut() {
        if let Some((raw, iso)) = *t {
            let tid = ThreadId::from_raw(raw);
            if iso {
                system::destroy_isolated(tid);
            } else {
                system::kill_local(tid);
            }
            *t = None;
        }
    }
    while system::reap() > 0 {}
    for m in maps.iter_mut() {
        *m = None;
    }
    // Jede noch gültige Cap revoken (Nachfahren weg) + löschen (Wurzel -> Frame frei).
    for c in caps.iter_mut() {
        if let Some(p) = *c {
            if system::cap_inspect(p).is_some() {
                let _ = system::cap_revoke(p);
                let _ = system::cap_delete(p);
            }
            *c = None;
        }
    }
}

/// **Fuzzer-Treiber** (EL1, core 0, prio 2 -> dominiert core 0 während des Fuzzings).
/// Phase 1: Epochen aus zufälligen Ops + Teardown + Oracle. Phase 2: SMP-Churn.
extern "C" fn fuzz_driver(_arg: usize) -> ! {
    let mut s = FUZZ_SEED;
    let mut caps: [Option<CapPtr>; FUZZ_CAP_POOL] = [None; FUZZ_CAP_POOL];
    let mut threads: [Option<(u64, bool)>; FUZZ_THREAD_POOL] = [None; FUZZ_THREAD_POOL];
    let mut maps: [Option<(u64, u64)>; FUZZ_MAP_POOL] = [None; FUZZ_MAP_POOL];

    // Baseline nach dem Leeren ausstehender Zombies.
    while system::reap() > 0 {}
    let base = fuzz_snapshot();

    let mut ok = true;
    for epoch in 0..FUZZ_EPOCHS {
        for _ in 0..FUZZ_OPS_PER_EPOCH {
            fuzz_step(&mut s, &mut caps, &mut threads, &mut maps);
            FUZZ_OPS.fetch_add(1, Ordering::Relaxed);
        }
        // CDT-/Refcount-Property MITTEN in der Epoche (Caps maximal abgeleitet) prüfen —
        // fängt Ableitungs-/Refcount-Fehler, die ein reiner Baseline-Vergleich verpasst.
        let cdt = system::cap_audit_cdt();
        if cdt != 0 {
            FUZZ_FAIL.store(40 + cdt, Ordering::Release); // 40+ = mid-epoch CDT-Bruch
            ok = false;
            break;
        }
        // VMM-Property MITTEN in der Epoche (isolierte VSpaces + Mappings aktiv) prüfen:
        // W^X + Struktur der Seitentabellen (Code 60+).
        let vmm = system::vspace_audit();
        if vmm != 0 {
            FUZZ_FAIL.store(60 + vmm, Ordering::Release);
            ok = false;
            break;
        }
        fuzz_teardown(&mut caps, &mut threads, &mut maps);
        while system::reap() > 0 {}
        if let Some(code) = fuzz_check(&base) {
            FUZZ_FAIL.store(code, Ordering::Release);
            ok = false;
            break;
        }
        FUZZ_EPOCHS_OK.store(epoch + 1, Ordering::Release);
    }

    // Phase 2: SMP-Churn. Noise-Worker auf cores 1..NUM_CORES; Baseline NACH dem Spawn
    // (Worker warten auf GO -> fester Footprint). Dann GO, Treiber-Churn auf core 0,
    // STOP, auf Quiesce aller Worker warten, Baseline-Check (alle Churn balanciert).
    let mut smp_workers = 0u32;
    for c in 1..system::num_cores() {
        if system::spawn_on_core(c, fuzz_noise as *const () as usize, 0, 5).is_some() {
            smp_workers += 1;
        }
    }
    let smp_base = fuzz_snapshot();
    FUZZ_SMP_GO.store(true, Ordering::Release);
    for _ in 0..FUZZ_SMP_ITERS {
        fuzz_noise_iter(); // core 0 churnt mit
    }
    FUZZ_SMP_STOP.store(true, Ordering::Release);
    // Auf Quiesce warten (bounded; TCG-Round-Robin lässt die anderen Kerne acken).
    let mut spins = 0u64;
    while FUZZ_SMP_ACK.load(Ordering::Acquire) < smp_workers && spins < 40_000_000 {
        core::hint::spin_loop();
        spins += 1;
    }
    while system::reap() > 0 {}
    let smp_ok = FUZZ_SMP_ACK.load(Ordering::Acquire) == smp_workers && fuzz_check(&smp_base).is_none();
    FUZZ_SMP_OK.store(smp_ok, Ordering::Release);
    FUZZ_OK.store(ok && smp_ok, Ordering::Release);
    FUZZ_DONE.store(true, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

/// Eine vollständig balancierte Cap/MEM-Churn-Iteration (netto 0): Frame -> Cap ->
/// Copy -> Revoke(Wurzel, löscht Copy) -> Delete(Wurzel, gibt Frame frei). Von core 0
/// (Treiber) und den Noise-Workern genutzt -> konkurrierender Zugriff auf CAPS+MEM.
fn fuzz_noise_iter() {
    if let Some(c) = system::alloc(4096, 4096) {
        match system::cap_install(c) {
            Ok(root) => {
                if system::cap_copy(root, Rights::READ).is_ok() {
                    let _ = system::cap_revoke(root);
                }
                let _ = system::cap_delete(root);
            }
            Err(_) => { /* cspace voll (unter beschränkter Last unerreichbar) */ }
        }
    }
    FUZZ_SMP_OPS.fetch_add(1, Ordering::Relaxed);
}

/// **SMP-Noise-Worker** (EL1, je Sekundärkern): wartet auf GO, churnt dann balanciert
/// (CAPS+MEM) bis STOP, quittiert (ACK) und parkt. Erzeugt echte Mehrkern-Kontention
/// auf den globalen CAPS-/MEM-Locks + interleavte CDT-Mutationen.
extern "C" fn fuzz_noise(_arg: usize) -> ! {
    while !FUZZ_SMP_GO.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    while !FUZZ_SMP_STOP.load(Ordering::Acquire) {
        fuzz_noise_iter();
    }
    FUZZ_SMP_ACK.fetch_add(1, Ordering::Release);
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

// ---------------------------------------------------------------------------
// IPC-State-Machine-Fuzzer — mehrere gleichzeitige Aktoren (Clients/Server/
// Notifier/Waiter) über alle Kerne + Controller, der zu ungünstigen Zeiten KILL/
// MCS-Bind/Cap-Churn injiziert und tote Aktoren respawnt. Pro Epoche läuft das
// strukturelle IPC-Oracle (`system::ipc_audit`): keine toten/duplizierten TCBs in
// Endpoint-/Notification-Queues, keine Ready-Queue-Korruption, keine verlorenen
// Threads. Am Ende vollständiger Teardown + Ressourcen-Baseline.
// ---------------------------------------------------------------------------
const IPCF_EPOCHS: u32 = 8;
const IPCF_EVENTS: u32 = 8; // Meta-Events je Epoche (einmalig, kein Retry)
// Spin-Lauffenster je Epoche: gibt den Aktoren auf cores 1..7 (TCG-Round-Robin)
// Zeit, IPC zu fahren. `spin_loop` ist unter TCG billig -> der Lauf bleibt schnell;
// größere Fenster -> mehr Kern-Wechsel -> mehr IPC-Ops. (Tick-basiertes Warten wäre
// load-pathologisch unter Mehrkern-TCG -> verworfen.) `system_off` beendet bei
// Abschluss, sodass das Test-Skript nicht bis zum Timeout warten muss.
const IPCF_DELAY: u32 = 250_000;
const IPCF_PRIO: u8 = 4; // Aktor-Priorität (dominiert die Sekundärkerne)
const IPCF_SEED: u64 = 0x19CF_2026_0624_0001;
const IPCF_N: usize = 10; // Aktoren gesamt (cores 1..7; core 0 = Controller)
static IPCFUZZ_DONE: AtomicBool = AtomicBool::new(false);
static IPCFUZZ_OK: AtomicBool = AtomicBool::new(false);
static IPCF_OPS: AtomicU64 = AtomicU64::new(0); // IPC-Operationen der Aktoren
static IPCF_KILLS: AtomicU64 = AtomicU64::new(0); // injizierte KILLs (Diagnose)
static IPCF_EPOCHS_OK: AtomicU32 = AtomicU32::new(0);
static IPCF_ANOMALY: AtomicU32 = AtomicU32::new(0); // Oracle-Code des ersten Bruchs
static IPCF_FAIL_EPOCH: AtomicU32 = AtomicU32::new(0);
static IPCF_TEARDOWN_OK: AtomicBool = AtomicBool::new(false);
static IPCF_STOP: AtomicBool = AtomicBool::new(false); // Teardown-Signal: Aktoren beenden sich

/// Ein Fuzzer-Aktor: fester Kern + PD + Einstiegsfunktion; `tid_raw` = aktuelle
/// (ggf. respawnte) Thread-Instanz.
#[derive(Clone, Copy)]
struct Actor {
    core: usize,
    pd: usize,
    entry: usize,
    arg: usize,
    tid_raw: u64,
}

/// **Server-Aktor**: RECV/REPLY-Schleife auf seinem Endpoint (lokaler Cap-Slot 0).
/// Gelegentlich YIELD zwischen RECV und REPLY (ungünstiger Zeitpunkt) oder Selbst-EXIT.
extern "C" fn ipcf_server(arg: usize) -> ! {
    let mut s = (arg as u64) | 1;
    loop {
        if IPCF_STOP.load(Ordering::Relaxed) {
            invoke(sys::EXIT, 0, [0; 4], 0);
        }
        let m = invoke(sys::RECV, 0, [0; 4], 0);
        if m.result == result::OK {
            IPCF_OPS.fetch_add(1, Ordering::Relaxed);
            if frand(&mut s) % 8 == 0 {
                invoke(sys::YIELD, 0, [0; 4], 0); // RECV..REPLY-Fenster vergrößern
            }
            invoke(sys::REPLY, 0, [m.msg[0].wrapping_add(1), 0, 0, 0], 0);
            IPCF_OPS.fetch_add(1, Ordering::Relaxed);
        } else {
            invoke(sys::YIELD, 0, [0; 4], 0); // Cap entzogen/Reload -> nicht busy-spinnen
        }
        if frand(&mut s) % 200 == 0 {
            invoke(sys::EXIT, 0, [0; 4], 0); // EXIT mitten im Betrieb
        }
    }
}

/// **Client-Aktor**: zufällig CALL auf Endpoint A/B (Cap-Slot 0/1), YIELD oder EXIT.
extern "C" fn ipcf_client(arg: usize) -> ! {
    let mut s = (arg as u64) | 1;
    loop {
        if IPCF_STOP.load(Ordering::Relaxed) {
            invoke(sys::EXIT, 0, [0; 4], 0);
        }
        match frand(&mut s) % 16 {
            0 => {
                invoke(sys::EXIT, 0, [0; 4], 0); // Selbst-EXIT (oft mitten im CALL-Zyklus)
            }
            1 => {
                invoke(sys::YIELD, 0, [0; 4], 0);
            }
            _ => {
                let slot = frand(&mut s) % 2; // EP A oder B
                let _ = invoke(sys::CALL, slot, [frand(&mut s), 0, 0, 0], 0);
                IPCF_OPS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// **Notifier-Aktor**: zufällig SIGNAL auf Notification A/B (Cap-Slot 0/1).
extern "C" fn ipcf_notifier(arg: usize) -> ! {
    let mut s = (arg as u64) | 1;
    loop {
        if IPCF_STOP.load(Ordering::Relaxed) {
            invoke(sys::EXIT, 0, [0; 4], 0);
        }
        let slot = frand(&mut s) % 2;
        invoke(sys::SIGNAL, slot, [0; 4], 0);
        IPCF_OPS.fetch_add(1, Ordering::Relaxed);
        if frand(&mut s) % 4 == 0 {
            invoke(sys::YIELD, 0, [0; 4], 0);
        }
        if frand(&mut s) % 200 == 0 {
            invoke(sys::EXIT, 0, [0; 4], 0);
        }
    }
}

/// **Waiter-Aktor**: WAIT auf Notification A/B (Cap-Slot 0/1) — blockiert bis Signal.
extern "C" fn ipcf_waiter(arg: usize) -> ! {
    let mut s = (arg as u64) | 1;
    loop {
        if IPCF_STOP.load(Ordering::Relaxed) {
            invoke(sys::EXIT, 0, [0; 4], 0);
        }
        let slot = frand(&mut s) % 2;
        invoke(sys::WAIT, slot, [0; 4], 0);
        IPCF_OPS.fetch_add(1, Ordering::Relaxed);
        if frand(&mut s) % 200 == 0 {
            invoke(sys::EXIT, 0, [0; 4], 0);
        }
    }
}

/// **Fast-Signaller**: SIGNALt eine Notification OHNE Wartenden (Cap-Slot 0) in einer
/// engen Schleife — asynchrones Senden ohne Kontextwechsel (nur Trap + Cap-Lookup +
/// `pending |= badge`). Liefert die hohe IPC-Op-Rate (Cross-Core-CALLs sind unter
/// Single-Thread-TCG zu teuer). Wird nicht gestört (kein KILL/MCS-Ziel) -> läuft voll.
extern "C" fn ipcf_signaller(_arg: usize) -> ! {
    loop {
        if IPCF_STOP.load(Ordering::Relaxed) {
            invoke(sys::EXIT, 0, [0; 4], 0);
        }
        invoke(sys::SIGNAL, 0, [0; 4], 0);
        IPCF_OPS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Snapshot für das Teardown-Oracle: alle Kerne summiert (Aktoren liegen auf 1..7).
fn ipcf_snapshot() -> (u64, usize, usize, usize, usize, usize) {
    let mut tcbs = 0;
    for c in 0..system::num_cores() {
        tcbs += system::used_tcbs(c);
    }
    (
        system::total_free(),
        tcbs,
        system::free_vspaces(),
        system::user_kstack_free_count(),
        system::cap_used_slots(),
        system::cap_used_objects(),
    )
}

/// Tote Aktoren (durch Controller-KILL oder Selbst-EXIT beendet) auf ihrem Kern neu
/// erzeugen + an ihre PD binden — hält die IPC-Last + die Death-during-IPC-Rennen am
/// Laufen.
fn ipcf_respawn(act: &mut [Actor; IPCF_N]) {
    for a in act.iter_mut() {
        if !system::thread_alive(ThreadId::from_raw(a.tid_raw)) {
            if let Some(t) = system::spawn_on_core_parked(a.core, a.entry, a.arg, IPCF_PRIO) {
                let _ = system::admit_in_pd(a.pd, t);
                a.tid_raw = t.to_raw();
            }
        }
    }
}

/// **IPC-Fuzzer-Controller** (EL1, core 0). Legt IPC-Objekte + Caps + Aktor-PDs an,
/// nimmt die Baseline, spawnt die Aktoren auf cores 1..7, fährt Epochen aus injizierten
/// Meta-Events (KILL/MCS-Bind/Cap-Churn) + Oracle, und baut am Ende alles ab + prüft
/// die Ressourcen-Baseline.
extern "C" fn ipcfuzz_controller(_arg: usize) -> ! {
    // --- IPC-Objekte + Wurzel-Caps + abgeleitete Caps ---
    let ep_a = system::create_endpoint().expect("ipcf ep a");
    let ep_b = system::create_endpoint().expect("ipcf ep b");
    let nt_a = system::create_notification().expect("ipcf nt a");
    let nt_b = system::create_notification().expect("ipcf nt b");
    let nt_c = system::create_notification().expect("ipcf nt c"); // für den Fast-Signaller (kein Waiter)
    let ra = system::install_endpoint_cap(ep_a as u32, Rights::RWX).expect("ipcf ra");
    let rb = system::install_endpoint_cap(ep_b as u32, Rights::RWX).expect("ipcf rb");
    let nca = system::install_notification_cap(nt_a as u32, Rights::RWX).expect("ipcf nca");
    let ncb = system::install_notification_cap(nt_b as u32, Rights::RWX).expect("ipcf ncb");
    let send_a = system::cap_mint(ra, Rights::WRITE, 0).expect("ipcf send a");
    let recv_a = system::cap_mint(ra, Rights::READ, 0).expect("ipcf recv a");
    let send_b = system::cap_mint(rb, Rights::WRITE, 0).expect("ipcf send b");
    let recv_b = system::cap_mint(rb, Rights::READ, 0).expect("ipcf recv b");
    // Badge != 0 (sonst gingen Signale vor dem WAIT verloren — bekannte Invariante).
    let sig_a = system::cap_mint(nca, Rights::WRITE, 0xA1).expect("ipcf sig a");
    let wait_a = system::cap_mint(nca, Rights::READ, 0).expect("ipcf wait a");
    let sig_b = system::cap_mint(ncb, Rights::WRITE, 0xB2).expect("ipcf sig b");
    let wait_b = system::cap_mint(ncb, Rights::READ, 0).expect("ipcf wait b");
    let ncc = system::install_notification_cap(nt_c as u32, Rights::RWX).expect("ipcf ncc");
    let sig_c = system::cap_mint(ncc, Rights::WRITE, 0xC3).expect("ipcf sig c");
    // Zwei wiederverwendbare SchedContext-Caps für MCS-Events: knapp (50 % Duty —
    // spürbare Drosselung, aber kein Verhungern) und unbeschränkt (Wiederherstellung).
    let sc_tiny = system::install_sched_context_cap(8, 16, Rights::WRITE).expect("ipcf sc tiny");
    let sc_full = system::install_sched_context_cap(0, 1, Rights::WRITE).expect("ipcf sc full");

    // --- Aktor-PDs + Cap-Installation ---
    let mk_pd = |caps: &[(usize, CapPtr)]| -> usize {
        let pd = system::create_pd().expect("ipcf pd");
        for &(slot, c) in caps {
            system::install_pd_cap(pd, slot, c);
        }
        pd
    };
    let pd_sa = mk_pd(&[(0, recv_a)]);
    let pd_sb = mk_pd(&[(0, recv_b)]);
    let pd_no = mk_pd(&[(0, sig_a), (1, sig_b)]);
    let pd_w0 = mk_pd(&[(0, wait_a), (1, wait_b)]);
    let pd_w1 = mk_pd(&[(0, wait_a), (1, wait_b)]);
    let pd_sg = mk_pd(&[(0, sig_c)]);
    let pd_c: [usize; 4] = core::array::from_fn(|_| mk_pd(&[(0, send_a), (1, send_b)]));

    let cli = ipcf_client as *const () as usize;
    let srv = ipcf_server as *const () as usize;
    let nof = ipcf_notifier as *const () as usize;
    let wai = ipcf_waiter as *const () as usize;
    let sgl = ipcf_signaller as *const () as usize;
    // Aktor-Tabelle (core, pd, entry, seed). Server auf 2/3, Clients 1/4/5/7, Notifier
    // 6, Waiter 6/1 -> alle Kerne 1..7 belegt, Cross-Core-IPC inhärent.
    let mut act: [Actor; IPCF_N] = [
        Actor { core: 2, pd: pd_sa, entry: srv, arg: 0x51, tid_raw: u64::MAX },
        Actor { core: 3, pd: pd_sb, entry: srv, arg: 0x52, tid_raw: u64::MAX },
        Actor { core: 1, pd: pd_c[0], entry: cli, arg: 0xC0, tid_raw: u64::MAX },
        Actor { core: 4, pd: pd_c[1], entry: cli, arg: 0xC1, tid_raw: u64::MAX },
        Actor { core: 5, pd: pd_c[2], entry: cli, arg: 0xC2, tid_raw: u64::MAX },
        Actor { core: 5, pd: pd_c[3], entry: cli, arg: 0xC3, tid_raw: u64::MAX },
        Actor { core: 6, pd: pd_no, entry: nof, arg: 0x6E, tid_raw: u64::MAX },
        Actor { core: 6, pd: pd_w0, entry: wai, arg: 0x70, tid_raw: u64::MAX },
        Actor { core: 1, pd: pd_w1, entry: wai, arg: 0x71, tid_raw: u64::MAX },
        // Index 9: Fast-Signaller (kein KILL-/MCS-Ziel) -> hohe async-IPC-Op-Rate.
        Actor { core: 7, pd: pd_sg, entry: sgl, arg: 0x59, tid_raw: u64::MAX },
    ];

    // --- Baseline (Objekte/Caps/PDs angelegt; noch keine Aktoren) ---
    while system::reap() > 0 {}
    let base = ipcf_snapshot();

    // --- Aktoren spawnen + binden ---
    for a in act.iter_mut() {
        if let Some(t) = system::spawn_on_core_parked(a.core, a.entry, a.arg, IPCF_PRIO) {
            let _ = system::admit_in_pd(a.pd, t);
            a.tid_raw = t.to_raw();
        }
    }

    // --- Epochen: Meta-Events injizieren + Oracle ---
    let mut s = IPCF_SEED;
    let mut ok = true;
    for epoch in 0..IPCF_EPOCHS {
        // Meta-Events EINMALIG injizieren (kein Retry-Spin -> minimaler Controller-
        // Overhead; ein verfehlter Kill — Ziel gerade laufend — wird in einer späteren
        // Runde oder im Teardown nachgeholt).
        for _ in 0..IPCF_EVENTS {
            match frand(&mut s) % 8 {
                0 | 1 | 2 => {
                    // KILL eines zufälligen Aktors (Death-during-IPC, auch cross-core).
                    // Index 0..8 (Server/Clients/Notifier/Waiter); der Fast-Signaller (9)
                    // bleibt verschont -> stabile Op-Rate.
                    let i = (frand(&mut s) % (IPCF_N as u64 - 1)) as usize;
                    if system::kill_remote(ThreadId::from_raw(act[i].tid_raw)) {
                        IPCF_KILLS.fetch_add(1, Ordering::Relaxed);
                    }
                }
                3 | 4 => {
                    // MCS: Budget an einen NICHT-Server (Index 2..8) binden — Server
                    // bleiben antwortbereit. Bias zu sc_full; 1/4 sc_tiny (Erschöpfung).
                    let i = 2 + (frand(&mut s) % (IPCF_N as u64 - 3)) as usize;
                    let tid = ThreadId::from_raw(act[i].tid_raw);
                    let sc = if frand(&mut s) % 4 == 0 { sc_tiny } else { sc_full };
                    if let Some(c) = system::owner_core_of(tid) {
                        system::bind_sched_context(sc, c, tid);
                    }
                }
                5 | 6 => {
                    // Balancierte Cap-Churn auf einer Kopie eines aktiv genutzten
                    // Endpoint-Caps (CDT/Refcount während IPC; netto baseline-neutral).
                    let root = if frand(&mut s) & 1 == 0 { ra } else { rb };
                    if let Ok(c) = system::cap_copy(root, Rights::READ) {
                        let _ = system::cap_revoke(c);
                        let _ = system::cap_delete(c);
                    }
                }
                _ => {
                    // HOT-RELOAD-Modell: einen Server quiescen (retire_receiver) + killen
                    // (-> ipcf_respawn bringt die neue Instanz auf demselben Endpoint).
                    let i = (frand(&mut s) % 2) as usize; // Server 0 (ep_a) / 1 (ep_b)
                    let ep = if i == 0 { ep_a } else { ep_b };
                    let tid = ThreadId::from_raw(act[i].tid_raw);
                    let _ = system::endpoint_retire_receiver(ep, tid);
                    if system::kill_remote(tid) {
                        IPCF_KILLS.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        ipcf_respawn(&mut act); // tote Aktoren neu erzeugen -> volle IPC-Last im Fenster
        // Lauffenster: die Aktoren auf cores 1..7 fahren IPC (TCG schaltet die Kerne
        // durch). Hier entsteht der Großteil der IPC-Operationen.
        for _ in 0..IPCF_DELAY {
            core::hint::spin_loop();
        }
        for c in 0..system::num_cores() {
            system::reap_core(c); // Zombies (gekillte Aktoren) aller Kerne einsammeln
        }
        // Oracle: strukturelle IPC-/Scheduler-Invarianten prüfen.
        let code = system::ipc_audit();
        if code != 0 {
            IPCF_ANOMALY.store(code, Ordering::Release);
            IPCF_FAIL_EPOCH.store(epoch, Ordering::Release);
            ok = false;
            break;
        }
        IPCF_EPOCHS_OK.store(epoch + 1, Ordering::Release);
    }

    // --- Teardown: STOP signalisieren (laufende Aktoren beenden sich selbst am
    // Schleifenkopf -> EXIT; blockierte werden gekillt), dann reapen + Baseline. ---
    IPCF_STOP.store(true, Ordering::Release);
    for a in act.iter() {
        let tid = ThreadId::from_raw(a.tid_raw);
        let mut tries = 0;
        while system::thread_alive(tid) && tries < 4000 {
            system::kill_remote(tid); // blockierte (nicht laufende) Aktoren töten
            for c in 0..system::num_cores() {
                system::reap_core(c);
            }
            for _ in 0..1500 {
                core::hint::spin_loop(); // laufende Aktoren erreichen den STOP-Check -> EXIT
            }
            tries += 1;
        }
    }
    // Restliche Zombies einsammeln + Baseline-Konvergenz abwarten (bounded).
    let mut spins = 0u32;
    loop {
        let mut z = 0;
        for c in 0..system::num_cores() {
            z += system::reap_core(c);
        }
        if (ipcf_snapshot() == base && z == 0) || spins >= 4000 {
            break;
        }
        spins += 1;
        for _ in 0..2000 {
            core::hint::spin_loop();
        }
    }
    let _ = spins;
    let teardown_ok = ipcf_snapshot() == base && system::ipc_audit() == 0;
    IPCF_TEARDOWN_OK.store(teardown_ok, Ordering::Release);
    IPCFUZZ_OK.store(ok && teardown_ok, Ordering::Release);
    IPCFUZZ_DONE.store(true, Ordering::Release);
    // Direkte Ergebnis-Zeile (unabhängig vom Gesamt-Report): bei einer Anomalie sind
    // Seed + Fehl-Epoche reproduzierbar.
    println!(
        "ipcfuzz : fertig — {} Epochen ok, {} IPC-Ops, {} KILLs, Oracle-Anomalie={} (Epoche {}), Teardown-Baseline={}, seed={:#x}",
        IPCF_EPOCHS_OK.load(Ordering::Relaxed),
        IPCF_OPS.load(Ordering::Relaxed),
        IPCF_KILLS.load(Ordering::Relaxed),
        IPCF_ANOMALY.load(Ordering::Relaxed),
        IPCF_FAIL_EPOCH.load(Ordering::Relaxed),
        teardown_ok,
        IPCF_SEED,
    );
    loop {
        invoke(sys::PARK, 0, [0; 4], 0);
    }
}

