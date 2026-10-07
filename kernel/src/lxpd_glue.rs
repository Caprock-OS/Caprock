//! **Stand-in for `lxpd_glue.rs`, which the base commit references but never contained.**
//!
//! `loader.rs` declares `#[path = "lxpd_glue.rs"] mod lxpd_glue;`, yet the file was never
//! committed on this lineage, so the aarch64 kernel did not build from a clean checkout. This
//! file implements exactly the API `loader::lxpd_container_gate` uses and exactly the behaviour
//! its doc comment states for boot time: there is no `verify_manifest` verdict, so the gate ends
//! in `Unverified` (mapped to `TransportNur`). When the real file lands, it replaces this one
//! (expect an add/add conflict on merge: take the real file).

pub const LXPD_SCHEMA_VERSION: u32 = 1;

pub struct LxpdFacts {
    pub verified: bool,
    pub signature_present: bool,
    pub schema_version: u32,
    pub trampoline_names: usize,
    pub tramp_count: usize,
    pub coverage_pct: f64,
    pub bar_base: u64,
    pub bar_size: u64,
    pub dma_base: u64,
    pub dma_size: u64,
    pub irq: u32,
    pub heap_pages: u64,
}

pub struct ExecAntrag {
    pub program_id: u32,
    pub epoche: u32,
    pub token: u64,
    pub eintrag: u64,
    pub segmente: usize,
}

pub struct LxpdPolicy {
    pub max_bar_bytes: u64,
    pub max_dma_bytes: u64,
    pub max_heap_pages: u64,
    pub exec: ExecAntrag,
}

#[allow(dead_code)]
pub enum LxpdSpawnError {
    Unverified,
    BadImage,
    BadGrants,
    Ueberlappung,
    BudgetErschoepft,
    ExecAbgelehnt,
}

pub fn teardown_token_fuer(pid: u32, epoche: u32) -> u64 {
    ((pid as u64) << 32) | epoche as u64
}

/// Fail closed: nothing is verified at boot, so nothing is admitted.
pub fn spawn_entscheidung(f: &LxpdFacts, _p: &LxpdPolicy) -> Result<(), LxpdSpawnError> {
    if !f.verified {
        return Err(LxpdSpawnError::Unverified);
    }
    Err(LxpdSpawnError::ExecAbgelehnt)
}
