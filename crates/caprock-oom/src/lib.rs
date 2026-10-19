//! **Out-of-memory policy core.**
//!
//! The kernel must keep running when physical memory is gone, and recovery must not stall the
//! system. This crate is the part of the kernel that decides *what to do* about memory pressure.
//! It is built so that it **cannot run out of memory itself**:
//!
//! * no allocation: all state is a fixed array sized by the const parameter `N` (the number of
//!   protection domains), so the whole state is a plain value that can live in a `static`;
//! * no `unsafe`, no dependencies, no recursion, every loop is bounded by `N`;
//! * all arithmetic saturates -- no input can panic or overflow.
//!
//! ## What it provides
//!
//! 1. **Pressure levels** ([`Level`]) with hysteresis, so a level does not flap around a watermark.
//! 2. **An emergency reserve**: memory that ordinary requests may never take
//!    ([`Oom::admit`]), so the services that run the recovery -- the root task, the supervisor, a
//!    driver the system cannot do without -- can still get the few objects they need.
//! 3. **Victim choice** ([`Oom::pick_victim`]): by class (declared in the signed manifest), then by
//!    memory held. Protected domains are never chosen. If there is no candidate the answer is
//!    `None`, and the kernel carries on: *no victim* is a state, not an error.
//! 4. **Sliced reaping** ([`Oom::reap_step`]): tearing a victim down is done in steps of bounded
//!    work, so the scheduler keeps running while memory comes back. A settle period after each
//!    kill avoids killing more domains than necessary.
//! 5. **Freeze instead of swap**: there is no swap. A victim that can be frozen (its state holds
//!    no device authority, see the kernel's `PdFreeze` refusals) is *frozen to disk* instead of
//!    killed, as long as the freeze space on disk has room for it
//!    ([`Disposition::Freeze`]). Its memory comes back like after a kill, but the program is not
//!    lost: it is thawed on demand ([`Oom::thaw`]).
//!
//! ## What it does not do
//!
//! It does not free anything itself and does not touch the allocator: the kernel calls
//! [`Oom::account_alloc`] / [`Oom::account_free`] from the paths that already know the numbers,
//! and performs the actual teardown, reporting progress back through [`Oom::reap_step`]. Policy
//! beyond the manifest classes (who is a good victim) belongs to a user-space supervisor, which is
//! told about pressure through a notification the kernel prepared at boot; this core is the
//! fallback that guarantees progress when no supervisor acts in time.
#![no_std]
#![forbid(unsafe_code)]

/// Memory pressure, least to most severe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Normal,
    Warning,
    Critical,
    /// Out of memory: ordinary requests are refused and reaping is due.
    Oom,
}

/// How expendable a domain is. Declared per program in the signed manifest, never chosen at run
/// time. Ordered so that a larger value is a better victim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Never killed, may use the reserve (root task, supervisor).
    Protected,
    /// May use the reserve, killed only when nothing less important is left.
    Essential,
    /// Ordinary program.
    Normal,
    /// Caches, previews, anything that can be restarted at no loss.
    Expendable,
}

/// Free-memory thresholds in bytes, strictly decreasing: `warning > critical > oom`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watermarks {
    pub warning: u64,
    pub critical: u64,
    pub oom: u64,
}

impl Watermarks {
    /// `true` if the thresholds are strictly decreasing.
    pub const fn valid(&self) -> bool {
        self.warning > self.critical && self.critical > self.oom
    }
}

/// Accounting for one domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account {
    /// Bytes of physical memory the domain currently holds.
    pub used: u64,
    pub class: Class,
    /// The domain exists. A dead slot is never a victim.
    pub alive: bool,
    /// The domain may be frozen to disk instead of killed.
    pub freezable: bool,
    /// Bytes this domain has on disk while frozen (0 = not frozen).
    pub frozen_bytes: u64,
}

impl Account {
    pub const EMPTY: Account = Account {
        used: 0,
        class: Class::Normal,
        alive: false,
        freezable: false,
        frozen_bytes: 0,
    };
}

/// What happens to a victim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The domain is destroyed.
    Kill,
    /// The domain's memory is written to the freeze space and released; it can be thawed.
    Freeze,
}

/// What [`Oom::admit`] says about an allocation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Go ahead.
    Allow,
    /// Refused for now, recovery is running: the caller should retry soon (like `ERR_QUIESCING`:
    /// "comes back shortly", not "does not exist").
    Retry,
    /// Refused: no memory and no recovery in sight.
    NoSpace,
}

/// The reaping state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nothing to do.
    Idle,
    /// A victim is being torn down; `steps` slices have been spent on it.
    Reaping { victim: usize, steps: u32, how: Disposition },
    /// A victim was fully reaped; wait `left` more ticks before judging whether another kill is
    /// needed, so memory the first kill is returning is not counted as still missing.
    Settling { left: u32 },
}

/// What one reaping step decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reap {
    /// Keep calling [`Oom::reap_step`] with the next slice's progress.
    Continue,
    /// The victim is gone (killed) or frozen to disk; its memory was released.
    Done { victim: usize, how: Disposition },
    /// The victim made no progress for too long; the kernel should force-finish it.
    Stuck { victim: usize },
    /// Nothing is being reaped.
    Idle,
}

/// Reaping limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Slices of no progress before a victim counts as stuck.
    pub stuck_steps: u32,
    /// Ticks to wait after a kill before the next judgement.
    pub settle_ticks: u32,
}

/// The out-of-memory core for `N` protection domains.
#[derive(Clone, Copy, Debug)]
pub struct Oom<const N: usize> {
    wm: Watermarks,
    /// Bytes above a watermark needed to leave its level again.
    hyst: u64,
    limits: Limits,
    reserve_total: u64,
    reserve_used: u64,
    level: Level,
    phase: Phase,
    pds: [Account; N],
    /// Slices a victim has gone without progress.
    idle_steps: u32,
    kills: u32,
    freezes: u32,
    refused: u32,
    /// Bytes still free in the freeze space on disk.
    freeze_left: u64,
}

impl<const N: usize> Oom<N> {
    /// A fresh core. `reserve` bytes are kept back from ordinary requests.
    pub const fn new(wm: Watermarks, hyst: u64, limits: Limits, reserve: u64) -> Self {
        Self {
            wm,
            hyst,
            limits,
            reserve_total: reserve,
            reserve_used: 0,
            level: Level::Normal,
            phase: Phase::Idle,
            pds: [Account::EMPTY; N],
            idle_steps: 0,
            kills: 0,
            freezes: 0,
            refused: 0,
            freeze_left: 0,
        }
    }

    /// Set how many bytes the freeze space on disk can still take (0 = freezing is off).
    pub fn set_freeze_space(&mut self, bytes: u64) {
        self.freeze_left = bytes;
    }
    pub const fn freeze_left(&self) -> u64 {
        self.freeze_left
    }
    pub const fn freezes(&self) -> u32 {
        self.freezes
    }

    pub const fn level(&self) -> Level {
        self.level
    }
    pub const fn phase(&self) -> Phase {
        self.phase
    }
    pub const fn kills(&self) -> u32 {
        self.kills
    }
    pub const fn refused(&self) -> u32 {
        self.refused
    }
    pub const fn reserve_left(&self) -> u64 {
        self.reserve_total.saturating_sub(self.reserve_used)
    }

    /// A domain starts (or restarts) with the given class; it cannot be frozen.
    pub fn domain_started(&mut self, pd: usize, class: Class) {
        self.domain_started_ex(pd, class, false);
    }

    /// Like [`domain_started`](Self::domain_started), saying whether the domain may be frozen.
    pub fn domain_started_ex(&mut self, pd: usize, class: Class, freezable: bool) {
        if pd < N {
            self.pds[pd] = Account { used: 0, class, alive: true, freezable, frozen_bytes: 0 };
        }
    }

    /// A domain ended; whatever it still held was freed with it.
    pub fn domain_ended(&mut self, pd: usize) {
        if pd < N {
            // A frozen domain that ends gives its disk space back.
            self.freeze_left = self.freeze_left.saturating_add(self.pds[pd].frozen_bytes);
            self.pds[pd] = Account::EMPTY;
        }
    }

    /// `bytes` of physical memory were given to `pd`.
    pub fn account_alloc(&mut self, pd: usize, bytes: u64) {
        if pd < N {
            self.pds[pd].used = self.pds[pd].used.saturating_add(bytes);
        }
    }

    /// `bytes` of physical memory came back from `pd`.
    pub fn account_free(&mut self, pd: usize, bytes: u64) {
        if pd < N {
            self.pds[pd].used = self.pds[pd].used.saturating_sub(bytes);
        }
    }

    /// Bytes held by `pd` (0 for an unknown domain).
    pub fn used(&self, pd: usize) -> u64 {
        if pd < N { self.pds[pd].used } else { 0 }
    }

    /// Feed the current amount of free memory. Returns the new level if it changed.
    ///
    /// Going **down** (worse) happens as soon as free memory is below a watermark. Going **up**
    /// (better) needs `hyst` bytes of margin above that watermark, so a level that sits right at
    /// a watermark does not flip back and forth.
    pub fn observe_free(&mut self, free: u64) -> Option<Level> {
        let worse = if free < self.wm.oom {
            Level::Oom
        } else if free < self.wm.critical {
            Level::Critical
        } else if free < self.wm.warning {
            Level::Warning
        } else {
            Level::Normal
        };
        let new = if worse > self.level {
            worse
        } else {
            // Only improve while clearly above the threshold of the level we are in.
            let leave_at = match self.level {
                Level::Normal => 0,
                Level::Warning => self.wm.warning.saturating_add(self.hyst),
                Level::Critical => self.wm.critical.saturating_add(self.hyst),
                Level::Oom => self.wm.oom.saturating_add(self.hyst),
            };
            if self.level > Level::Normal && free >= leave_at { worse } else { self.level }
        };
        if new != self.level {
            self.level = new;
            Some(new)
        } else {
            None
        }
    }

    /// Decide an allocation request of `size` bytes by a domain of class `class`, given `free`
    /// bytes of free memory.
    ///
    /// * Ordinary classes may only use memory **above the reserve**: they are refused when
    ///   `free - size` would dip below it.
    /// * `Protected` and `Essential` may dip into the reserve, as long as the reserve lasts. This
    ///   is the lane that keeps the recovery services alive.
    /// * A refusal is [`Verdict::Retry`] while reaping or settling (memory is coming back) and
    ///   [`Verdict::NoSpace`] otherwise.
    pub fn admit(&mut self, class: Class, size: u64, free: u64) -> Verdict {
        let reserve_floor = self.reserve_left();
        let after = free.checked_sub(size);
        let privileged = matches!(class, Class::Protected | Class::Essential);
        let ok = match after {
            None => false,
            Some(rest) if privileged => {
                // May use the reserve: allowed while the request fits at all.
                let _ = rest;
                true
            }
            Some(rest) => rest >= reserve_floor,
        };
        if ok {
            if privileged {
                // Charge the part of the request that cuts into the reserve.
                if let Some(rest) = after {
                    if rest < reserve_floor {
                        let cut = reserve_floor - rest;
                        self.reserve_used = self.reserve_used.saturating_add(cut).min(self.reserve_total);
                    }
                }
            }
            Verdict::Allow
        } else {
            self.refused = self.refused.saturating_add(1);
            match self.phase {
                Phase::Reaping { .. } | Phase::Settling { .. } => Verdict::Retry,
                Phase::Idle => Verdict::NoSpace,
            }
        }
    }

    /// Give reserve bytes back (memory the privileged lane no longer holds).
    pub fn reserve_returned(&mut self, bytes: u64) {
        self.reserve_used = self.reserve_used.saturating_sub(bytes);
    }

    /// The best victim: the most expendable class first, then the largest holder, then the
    /// lowest index. `Protected`, dead and empty domains are never chosen. `None` means there is
    /// no candidate -- the kernel keeps running without killing anything.
    pub fn pick_victim(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for i in 0..N {
            let a = &self.pds[i];
            if !a.alive || a.class == Class::Protected || a.used == 0 {
                continue;
            }
            best = match best {
                None => Some(i),
                Some(b) => {
                    let bb = &self.pds[b];
                    if a.class > bb.class || (a.class == bb.class && a.used > bb.used) {
                        Some(i)
                    } else {
                        Some(b)
                    }
                }
            };
        }
        best
    }

    /// Start reaping if memory is out and nothing is being reaped. Returns the chosen victim and
    /// what will happen to it.
    ///
    /// Does nothing (returns `None`) unless the level is [`Level::Oom`], the phase is idle, and a
    /// candidate exists. The victim is **frozen** if it is freezable and the freeze space can take
    /// everything it holds, otherwise **killed**. The freeze space is reserved at this point.
    pub fn begin_reap(&mut self) -> Option<(usize, Disposition)> {
        if self.level != Level::Oom || self.phase != Phase::Idle {
            return None;
        }
        let v = self.pick_victim()?;
        let a = self.pds[v];
        let how = if a.freezable && a.frozen_bytes == 0 && a.used <= self.freeze_left {
            self.freeze_left -= a.used;
            Disposition::Freeze
        } else {
            Disposition::Kill
        };
        self.phase = Phase::Reaping { victim: v, steps: 0, how };
        self.idle_steps = 0;
        Some((v, how))
    }

    /// Report one slice of teardown (or write-out) work: `freed` bytes returned, `finished` if the
    /// victim is done. Call once per slice.
    pub fn reap_step(&mut self, freed: u64, finished: bool) -> Reap {
        let Phase::Reaping { victim, steps, how } = self.phase else {
            return Reap::Idle;
        };
        let before = self.pds[victim].used;
        self.account_free(victim, freed);
        if finished {
            match how {
                Disposition::Kill => {
                    self.pds[victim] = Account::EMPTY;
                    self.kills = self.kills.saturating_add(1);
                }
                Disposition::Freeze => {
                    // Everything the domain held moved to disk; the account stays, empty in RAM.
                    let a = &mut self.pds[victim];
                    a.frozen_bytes = before;
                    a.used = 0;
                    self.freezes = self.freezes.saturating_add(1);
                }
            }
            self.phase = Phase::Settling { left: self.limits.settle_ticks };
            return Reap::Done { victim, how };
        }
        if freed == 0 {
            self.idle_steps = self.idle_steps.saturating_add(1);
        } else {
            self.idle_steps = 0;
        }
        self.phase = Phase::Reaping { victim, steps: steps.saturating_add(1), how };
        if self.idle_steps >= self.limits.stuck_steps {
            Reap::Stuck { victim }
        } else {
            Reap::Continue
        }
    }

    /// How many bytes of RAM a thaw of `pd` needs (`None` if it is not frozen). The caller asks
    /// [`admit`](Self::admit) for exactly that, so a thaw competes for memory like any request.
    pub fn thaw_needs(&self, pd: usize) -> Option<u64> {
        (pd < N && self.pds[pd].alive && self.pds[pd].frozen_bytes > 0)
            .then(|| self.pds[pd].frozen_bytes)
    }

    /// The domain was read back from disk into RAM: its account holds the bytes again and the
    /// freeze space is released.
    pub fn thawed(&mut self, pd: usize) {
        if pd < N && self.pds[pd].frozen_bytes > 0 {
            let b = self.pds[pd].frozen_bytes;
            self.pds[pd].used = self.pds[pd].used.saturating_add(b);
            self.pds[pd].frozen_bytes = 0;
            self.freeze_left = self.freeze_left.saturating_add(b);
        }
    }

    /// One tick of the settle period. After it ends the phase is idle again, and the caller
    /// should [`observe_free`](Self::observe_free) and, if still out of memory, [`begin_reap`](Self::begin_reap).
    pub fn settle_tick(&mut self) {
        if let Phase::Settling { left } = self.phase {
            self.phase = if left <= 1 { Phase::Idle } else { Phase::Settling { left: left - 1 } };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WM: Watermarks = Watermarks { warning: 1000, critical: 500, oom: 100 };
    const LIM: Limits = Limits { stuck_steps: 3, settle_ticks: 2 };

    fn core<const N: usize>() -> Oom<N> {
        Oom::new(WM, 50, LIM, 64)
    }

    #[test]
    fn watermarks_must_be_strictly_decreasing() {
        assert!(WM.valid());
        assert!(!Watermarks { warning: 10, critical: 10, oom: 1 }.valid());
        assert!(!Watermarks { warning: 10, critical: 20, oom: 1 }.valid());
    }

    #[test]
    fn the_whole_state_is_a_plain_value_without_heap() {
        // Fixed size, Copy: it can live in a `static` and be copied around without allocating.
        fn is_copy<T: Copy>() {}
        is_copy::<Oom<64>>();
        assert!(core::mem::size_of::<Oom<64>>() < 4096);
    }

    #[test]
    fn levels_get_worse_at_once_and_better_only_with_margin() {
        let mut o = core::<4>();
        assert_eq!(o.observe_free(5000), None);
        assert_eq!(o.observe_free(900), Some(Level::Warning));
        assert_eq!(o.observe_free(90), Some(Level::Oom)); // skips levels downwards
        // Just above the oom watermark: not enough margin (needs 100 + 50).
        assert_eq!(o.observe_free(120), None);
        assert_eq!(o.level(), Level::Oom);
        // With margin it improves -- to the level the free memory actually justifies.
        assert_eq!(o.observe_free(200), Some(Level::Critical));
        assert_eq!(o.observe_free(100_000), Some(Level::Normal));
    }

    #[test]
    fn a_level_at_a_watermark_does_not_flap() {
        let mut o = core::<2>();
        o.observe_free(990); // Warning
        let mut changes = 0;
        for i in 0..1000 {
            // Hover around the warning watermark.
            let free = if i % 2 == 0 { 995 } else { 1005 };
            if o.observe_free(free).is_some() {
                changes += 1;
            }
        }
        assert_eq!(changes, 0, "hysteresis keeps the level steady");
    }

    #[test]
    fn ordinary_requests_cannot_touch_the_reserve() {
        let mut o = core::<2>();
        // free = 100, reserve 64: an ordinary request may use at most 36 bytes.
        assert_eq!(o.admit(Class::Normal, 36, 100), Verdict::Allow);
        assert_eq!(o.admit(Class::Normal, 37, 100), Verdict::NoSpace);
        assert_eq!(o.reserve_left(), 64);
    }

    #[test]
    fn the_privileged_lane_uses_the_reserve_and_it_runs_out() {
        let mut o = core::<2>();
        assert_eq!(o.admit(Class::Protected, 100, 100), Verdict::Allow);
        assert_eq!(o.reserve_left(), 0, "the whole reserve was cut into");
        // More than is free: refused even for the privileged lane.
        assert_eq!(o.admit(Class::Essential, 1, 0), Verdict::NoSpace);
        o.reserve_returned(64);
        assert_eq!(o.reserve_left(), 64);
    }

    #[test]
    fn refusal_says_retry_while_recovery_runs() {
        let mut o = core::<2>();
        o.domain_started(0, Class::Expendable);
        o.account_alloc(0, 10);
        o.observe_free(10);
        assert_eq!(o.begin_reap(), Some((0, Disposition::Kill)));
        assert_eq!(o.admit(Class::Normal, 50, 10), Verdict::Retry);
    }

    #[test]
    fn victim_is_the_most_expendable_then_the_largest() {
        let mut o = core::<5>();
        o.domain_started(0, Class::Protected);
        o.account_alloc(0, 10_000);
        o.domain_started(1, Class::Normal);
        o.account_alloc(1, 900);
        o.domain_started(2, Class::Expendable);
        o.account_alloc(2, 10);
        o.domain_started(3, Class::Expendable);
        o.account_alloc(3, 20);
        o.domain_started(4, Class::Essential);
        o.account_alloc(4, 5_000);
        // Expendable beats Normal beats Essential; among Expendable the larger holder wins.
        assert_eq!(o.pick_victim(), Some(3));
        o.domain_ended(3);
        assert_eq!(o.pick_victim(), Some(2));
        o.domain_ended(2);
        assert_eq!(o.pick_victim(), Some(1));
        o.domain_ended(1);
        assert_eq!(o.pick_victim(), Some(4));
        o.domain_ended(4);
        // Only the protected domain is left: no victim, and that is fine.
        assert_eq!(o.pick_victim(), None);
    }

    #[test]
    fn a_protected_domain_is_never_a_victim() {
        let mut o = core::<1>();
        o.domain_started(0, Class::Protected);
        o.account_alloc(0, u64::MAX);
        o.observe_free(0);
        assert_eq!(o.begin_reap(), None);
        assert_eq!(o.phase(), Phase::Idle, "no victim: the kernel simply carries on");
    }

    #[test]
    fn reaping_runs_in_slices_then_settles_then_idles() {
        let mut o = core::<2>();
        o.domain_started(0, Class::Normal);
        o.account_alloc(0, 300);
        o.observe_free(50);
        assert_eq!(o.begin_reap(), Some((0, Disposition::Kill)));
        assert_eq!(o.reap_step(100, false), Reap::Continue);
        assert_eq!(o.used(0), 200);
        assert_eq!(o.reap_step(200, true), Reap::Done { victim: 0, how: Disposition::Kill });
        assert_eq!(o.kills(), 1);
        assert_eq!(o.phase(), Phase::Settling { left: 2 });
        // While settling, no new victim is started even though memory is still out.
        assert_eq!(o.begin_reap(), None);
        o.settle_tick();
        assert_eq!(o.phase(), Phase::Settling { left: 1 });
        o.settle_tick();
        assert_eq!(o.phase(), Phase::Idle);
    }

    #[test]
    fn a_victim_that_makes_no_progress_is_reported_stuck() {
        let mut o = core::<1>();
        o.domain_started(0, Class::Normal);
        o.account_alloc(0, 10);
        o.observe_free(0);
        o.begin_reap();
        assert_eq!(o.reap_step(0, false), Reap::Continue);
        assert_eq!(o.reap_step(0, false), Reap::Continue);
        assert_eq!(o.reap_step(0, false), Reap::Stuck { victim: 0 });
        // Progress resets the counter.
        let mut p = core::<1>();
        p.domain_started(0, Class::Normal);
        p.account_alloc(0, 10);
        p.observe_free(0);
        p.begin_reap();
        p.reap_step(0, false);
        p.reap_step(0, false);
        assert_eq!(p.reap_step(1, false), Reap::Continue);
        assert_eq!(p.reap_step(0, false), Reap::Continue);
    }

    #[test]
    fn reaping_does_not_start_before_oom() {
        let mut o = core::<1>();
        o.domain_started(0, Class::Expendable);
        o.account_alloc(0, 10);
        o.observe_free(400); // Critical, not Oom
        assert_eq!(o.level(), Level::Critical);
        assert_eq!(o.begin_reap(), None);
    }

    #[test]
    fn out_of_range_domains_and_extreme_values_never_panic() {
        let mut o = core::<2>();
        o.domain_started(9, Class::Normal);
        o.domain_ended(9);
        o.account_alloc(9, 5);
        o.account_free(9, 5);
        assert_eq!(o.used(9), 0);
        o.domain_started(0, Class::Normal);
        o.account_alloc(0, u64::MAX);
        o.account_alloc(0, u64::MAX);
        assert_eq!(o.used(0), u64::MAX);
        o.account_free(0, u64::MAX);
        o.account_free(0, u64::MAX);
        assert_eq!(o.used(0), 0);
        assert_eq!(o.admit(Class::Normal, u64::MAX, 0), Verdict::NoSpace);
        assert_eq!(o.admit(Class::Protected, u64::MAX, u64::MAX), Verdict::Allow);
        assert!(o.reserve_left() <= 64);
        o.observe_free(u64::MAX);
        o.observe_free(0);
        let _ = o.reap_step(u64::MAX, true);
    }

    #[test]
    fn a_freezable_victim_is_frozen_not_killed_when_the_disk_has_room() {
        let mut o = core::<2>();
        o.domain_started_ex(0, Class::Normal, true);
        o.account_alloc(0, 300);
        o.set_freeze_space(1000);
        o.observe_free(10);
        assert_eq!(o.begin_reap(), Some((0, Disposition::Freeze)));
        assert_eq!(o.freeze_left(), 700, "the space is reserved up front");
        assert_eq!(o.reap_step(300, true), Reap::Done { victim: 0, how: Disposition::Freeze });
        assert_eq!(o.used(0), 0, "the memory is back");
        assert_eq!(o.kills(), 0);
        assert_eq!(o.freezes(), 1);
        assert_eq!(o.thaw_needs(0), Some(300), "but the program is not lost");
    }

    #[test]
    fn without_disk_room_or_freezability_the_victim_is_killed() {
        let mut full = core::<1>();
        full.domain_started_ex(0, Class::Normal, true);
        full.account_alloc(0, 300);
        full.set_freeze_space(299); // one byte short
        full.observe_free(10);
        assert_eq!(full.begin_reap(), Some((0, Disposition::Kill)));
        assert_eq!(full.freeze_left(), 299, "nothing was reserved");

        let mut no = core::<1>();
        no.domain_started_ex(0, Class::Normal, false);
        no.account_alloc(0, 10);
        no.set_freeze_space(u64::MAX);
        no.observe_free(10);
        assert_eq!(no.begin_reap(), Some((0, Disposition::Kill)));
    }

    #[test]
    fn thawing_gives_the_disk_space_back_and_a_frozen_domain_is_not_picked_again() {
        let mut o = core::<1>();
        o.domain_started_ex(0, Class::Normal, true);
        o.account_alloc(0, 300);
        o.set_freeze_space(1000);
        o.observe_free(10);
        o.begin_reap();
        o.reap_step(300, true);
        assert_eq!(o.pick_victim(), None, "empty in RAM, so nothing left to take");
        o.thawed(0);
        assert_eq!(o.used(0), 300);
        assert_eq!(o.freeze_left(), 1000);
        assert_eq!(o.thaw_needs(0), None);
    }

    #[test]
    fn a_frozen_domain_that_ends_returns_its_disk_space() {
        let mut o = core::<1>();
        o.domain_started_ex(0, Class::Normal, true);
        o.account_alloc(0, 300);
        o.set_freeze_space(1000);
        o.observe_free(10);
        o.begin_reap();
        o.reap_step(300, true);
        assert_eq!(o.freeze_left(), 700);
        o.domain_ended(0);
        assert_eq!(o.freeze_left(), 1000);
    }

    /// A long randomised run: the invariants hold whatever order things happen in.
    #[test]
    fn invariants_hold_over_a_long_random_run() {
        const N: usize = 8;
        let mut o = Oom::<N>::new(WM, 50, LIM, 64);
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let classes = [Class::Protected, Class::Essential, Class::Normal, Class::Expendable];
        for _ in 0..200_000 {
            let r = next();
            let pd = (r % (N as u64 + 2)) as usize; // sometimes out of range
            match (r >> 8) % 9 {
                0 => o.domain_started(pd, classes[((r >> 16) % 4) as usize]),
                1 => o.domain_ended(pd),
                2 => o.account_alloc(pd, (r >> 20) % 2000),
                3 => o.account_free(pd, (r >> 20) % 2000),
                4 => {
                    let _ = o.observe_free((r >> 24) % 3000);
                }
                5 => {
                    let _ = o.admit(classes[((r >> 16) % 4) as usize], (r >> 20) % 400, (r >> 32) % 800);
                }
                6 => {
                    let _ = o.begin_reap();
                }
                7 => {
                    let _ = o.reap_step((r >> 20) % 300, (r >> 40) % 5 == 0);
                }
                _ => o.settle_tick(),
            }
            // Invariants.
            assert!(o.reserve_left() <= 64);
            if let Some(v) = o.pick_victim() {
                assert!(v < N);
                assert!(o.pds[v].alive);
                assert_ne!(o.pds[v].class, Class::Protected);
                assert!(o.pds[v].used > 0);
            }
            if let Phase::Reaping { victim, .. } = o.phase() {
                assert!(victim < N);
            }
        }
    }
}
