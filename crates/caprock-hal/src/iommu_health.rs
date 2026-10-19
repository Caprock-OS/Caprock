//! **Arch-neutral IOMMU health — one statement, two implementations.**
//!
//! Until now each architecture reported the health of its own unit in its own words: aarch64
//! printed `smmu` (IDR0, SIDSIZE, SMMUEN, CMD_SYNC round trip, event queue, GERROR), x86 printed
//! `iommu`/`vtdcaps`/`vtdgrp` (version, CAP, GSTS.TES, context-cache acknowledgement). Both
//! implementations are complete — but there was **no single sentence that means the same thing on
//! both sides**, and `TODO.md` had named that in advance as the warning sign: *"a separate
//! `vtdtest` would mean the properties are worded differently on x86, and then a second design
//! does exist after all."*
//!
//! This module is that one sentence. It is a **pure type over injected values** — it reads no
//! register and calls into no architecture module, which is what makes it host-testable on its own
//! (the pattern of `dmar.rs` and `irte.rs`; `caprock-hal` as a whole never builds on the host).
//! The two facades in `x86_64/iommu.rs` and `aarch64/iommu.rs` gather the values and hand them in.
//!
//! ## The load-bearing rule of this module
//!
//! **`faults_empty` on its own is not a result.** An IOMMU that is wedged, unconfigured or simply
//! not answering also reports an empty fault queue — that is exactly the trap this project has
//! already paid for twice (SMMUv3 without `CD.R`: the event queue is *structurally* empty, and an
//! empty buffer looks like "no errors"; and virtio-rng, which only ever *wrote*, so the read
//! direction was never exercised).
//!
//! Therefore [`IommuHealth::ok`] weighs `faults_empty` **only** when
//! `invalidation_round_trip` is true. That flag is the liveness proof: the unit was given an
//! invalidation and it **acknowledged completion** — on aarch64 through the `CMD_SYNC` round trip,
//! on x86 through the QI wait descriptor writing its status word (or, while QI is off, `CCMD.ICC`
//! being cleared again). A unit that acknowledges is a unit that could have spoken.

/// The health of the platform's IOMMU, in terms both architectures can fill in.
///
/// Every field is a **measured** value, never a default. `present == false` means the platform has
/// no IOMMU; it is a legitimate outcome, not a failure — but then no other field carries meaning,
/// and [`ok`](Self::ok) says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IommuHealth {
    /// Is there a translation unit at all? On x86 that is an ACPI DMAR with at least one DRHD, on
    /// aarch64 a readable SMMUv3 IDR0.
    pub present: bool,
    /// Is translation switched on? (`GSTS.TES` / `SMMU_CR0.SMMUEN`.)
    ///
    /// **`false` does not mean "blocked", it means "free DMA".** That is the x86 trap from the
    /// register list — `TE = 0` lets every device through untranslated.
    pub translation_enabled: bool,
    /// Units found. aarch64 has exactly one SMMUv3; x86 may have several DRHDs.
    pub units: u32,
    /// Units that answer a read of their identity register. A silent unit cannot acknowledge
    /// anything, so it must not be counted as healthy.
    pub units_speaking: u32,
    /// **The liveness proof.** An invalidation was issued and the unit acknowledged its
    /// completion. Without this every other observation below is unfounded — see the module doc.
    pub invalidation_round_trip: bool,
    /// No recorded translation faults.
    ///
    /// Only meaningful together with `invalidation_round_trip`; on its own it is also what a dead
    /// unit reports.
    pub faults_empty: bool,
    /// Observation-independent counter for states after which "no faults" means nothing:
    /// configuration errors (the unit rejects our tables) and overflow of the fault recording.
    pub config_errors: u32,
    /// The unit's own error register (`GERROR` / `FSTS`). Nonzero is a hardware-side complaint
    /// about us and is never acceptable.
    pub hw_error: u32,
}

/// Why the health check did not pass — a **named** reason, so that a failing line says what is
/// wrong instead of only that something is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unhealthy {
    /// No unit on this platform. Not a defect; the caller decides whether it is acceptable.
    NoUnit,
    /// Units found but at least one does not answer.
    UnitSilent { speaking: u32, of: u32 },
    /// Translation is off — which means DMA is **free**, not blocked.
    TranslationOff,
    /// The unit never acknowledged an invalidation. Every statement about faults is therefore
    /// unfounded, including a green one.
    NoRoundTrip,
    /// The unit rejects our configuration, or the fault recording overflowed.
    ConfigErrors(u32),
    /// The unit's error register is set.
    HardwareError(u32),
    /// Faults are recorded — and because the round trip holds, that observation carries.
    FaultsRecorded,
}

impl IommuHealth {
    /// The value for a platform without an IOMMU. Everything else stays `false`/`0` so that no
    /// field can be read as a measurement that never happened.
    pub const ABSENT: Self = Self {
        present: false,
        translation_enabled: false,
        units: 0,
        units_speaking: 0,
        invalidation_round_trip: false,
        faults_empty: false,
        config_errors: 0,
        hw_error: 0,
    };

    /// The judgement, with a named reason on failure.
    ///
    /// The order of the checks is deliberate and is the content of this module: the liveness proof
    /// is weighed **before** the fault observation, because without it the fault observation is
    /// not evidence. Swapping the two would let a dead unit pass.
    pub fn verdict(&self) -> Result<(), Unhealthy> {
        if !self.present {
            return Err(Unhealthy::NoUnit);
        }
        if self.units_speaking != self.units {
            return Err(Unhealthy::UnitSilent { speaking: self.units_speaking, of: self.units });
        }
        if !self.translation_enabled {
            return Err(Unhealthy::TranslationOff);
        }
        if !self.invalidation_round_trip {
            return Err(Unhealthy::NoRoundTrip);
        }
        if self.config_errors != 0 {
            return Err(Unhealthy::ConfigErrors(self.config_errors));
        }
        if self.hw_error != 0 {
            return Err(Unhealthy::HardwareError(self.hw_error));
        }
        if !self.faults_empty {
            return Err(Unhealthy::FaultsRecorded);
        }
        Ok(())
    }

    /// Short form of [`verdict`](Self::verdict).
    pub fn ok(&self) -> bool {
        self.verdict().is_ok()
    }

    /// **Was this unit able to speak at all?** Separate from [`ok`](Self::ok) on purpose: a report
    /// line that judges absence has to be able to show it was capable of an utterance. A run in
    /// which this is `false` and `faults_empty` is `true` is not a pass — it is an empty
    /// measurement, and the two must be distinguishable in the log.
    pub fn speaking(&self) -> bool {
        self.present && self.units_speaking == self.units && self.invalidation_round_trip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A healthy unit, as both architectures deliver it on the standard setup.
    fn healthy() -> IommuHealth {
        IommuHealth {
            present: true,
            translation_enabled: true,
            units: 1,
            units_speaking: 1,
            invalidation_round_trip: true,
            faults_empty: true,
            config_errors: 0,
            hw_error: 0,
        }
    }

    #[test]
    fn healthy_passes() {
        assert_eq!(healthy().verdict(), Ok(()));
        assert!(healthy().speaking());
    }

    /// **The point of the whole module.** A dead unit reports an empty fault queue. Without the
    /// round trip that must not pass — otherwise silence is booked as success.
    #[test]
    fn empty_faults_without_round_trip_is_not_a_pass() {
        let mut h = healthy();
        h.invalidation_round_trip = false;
        assert_eq!(h.verdict(), Err(Unhealthy::NoRoundTrip));
        assert!(!h.speaking(), "a unit that never acknowledged is not speaking");
        assert!(h.faults_empty, "and it says so while its fault queue looks perfect");
    }

    /// Translation off is **free DMA**, not blocked DMA — the x86 `TE = 0` trap.
    #[test]
    fn translation_off_fails() {
        let mut h = healthy();
        h.translation_enabled = false;
        assert_eq!(h.verdict(), Err(Unhealthy::TranslationOff));
    }

    /// A silent unit among several is caught, and the reason names the numbers.
    #[test]
    fn a_silent_unit_among_several_is_caught() {
        let mut h = healthy();
        h.units = 3;
        h.units_speaking = 2;
        assert_eq!(h.verdict(), Err(Unhealthy::UnitSilent { speaking: 2, of: 3 }));
    }

    #[test]
    fn config_errors_and_hardware_errors_are_distinguishable() {
        let mut h = healthy();
        h.config_errors = 2;
        assert_eq!(h.verdict(), Err(Unhealthy::ConfigErrors(2)));
        let mut h = healthy();
        h.hw_error = 0x40;
        assert_eq!(h.verdict(), Err(Unhealthy::HardwareError(0x40)));
    }

    /// Recorded faults fail — and they only fail *because* the round trip holds, i.e. the
    /// observation is founded.
    #[test]
    fn recorded_faults_fail_and_the_observation_is_founded() {
        let mut h = healthy();
        h.faults_empty = false;
        assert_eq!(h.verdict(), Err(Unhealthy::FaultsRecorded));
        assert!(h.speaking());
    }

    /// A platform without an IOMMU is a named outcome, not a silent zero.
    #[test]
    fn absent_is_named() {
        assert_eq!(IommuHealth::ABSENT.verdict(), Err(Unhealthy::NoUnit));
        assert!(!IommuHealth::ABSENT.speaking());
    }

    /// **Speaking test of the judgement itself, in both directions.** A predicate that cannot
    /// fail is not a check; one that cannot pass is not one either.
    #[test]
    fn the_judgement_can_both_pass_and_fail() {
        assert!(healthy().ok(), "it must be able to pass");
        let mut broken = healthy();
        broken.hw_error = 1;
        assert!(!broken.ok(), "it must be able to fail");
    }

    /// **The ordering is the content.** With several defects at once the reason reported is the
    /// one that undermines the others — otherwise a log would name a fault while the unit was
    /// never alive to observe it.
    #[test]
    fn the_undermining_reason_wins() {
        let h = IommuHealth {
            present: true,
            translation_enabled: true,
            units: 1,
            units_speaking: 1,
            invalidation_round_trip: false,
            faults_empty: false,
            config_errors: 3,
            hw_error: 9,
        };
        assert_eq!(
            h.verdict(),
            Err(Unhealthy::NoRoundTrip),
            "without the liveness proof neither the fault count nor the error register is evidence"
        );
    }
}
