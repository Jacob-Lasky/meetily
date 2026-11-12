//! The ONE owner of the microphone and system stream slots (W93, cycle 2).
//!
//! Three independent actors touch these slots: start/stop, the mic hot-swap,
//! and dead-stream recovery. Patching each interleaving separately kept
//! producing a new race, so all mutations now go through this type, which
//! gives every slot an **epoch**:
//!
//! * start, stop and the swap (`swap_take`, `swap_install`) bump the epoch.
//! * recovery captures the epoch when it takes the dead stream
//!   (`recovery_take`) and may only install while the epoch is unchanged
//!   (`recovery_install`). Anything else having touched the slot makes the run
//!   obsolete: it abandons silently, never announces.
//! * every install (start, swap, recovery) reports a stream that is ALREADY
//!   dead on arrival (its failure event fired while it was not in a slot and
//!   was discarded as stale), so a stream installed by any path is covered by
//!   the same supervision and no path needs its own recheck.
//!
//! Generic over the stream type so the rules are testable without audio
//! hardware; production uses `AudioStream`.

use super::recording_state::{DeviceType, StreamFailure};

/// What the slots need to know about a stream.
pub trait Supervised {
    /// Unique per opened stream (`AudioCapture::stream_id`); failure events name it.
    fn stream_id(&self) -> u64;
    /// The stream's error gate already escalated: it is dead.
    fn is_dead(&self) -> bool;
}

struct Slot<S> {
    stream: Option<S>,
    epoch: u64,
}

impl<S> Slot<S> {
    fn new() -> Self {
        Self { stream: None, epoch: 0 }
    }
}

pub struct StreamSlots<S> {
    mic: Slot<S>,
    system: Slot<S>,
}

impl<S: Supervised> StreamSlots<S> {
    pub fn new() -> Self {
        Self { mic: Slot::new(), system: Slot::new() }
    }

    fn slot(&self, kind: DeviceType) -> &Slot<S> {
        match kind {
            DeviceType::Microphone => &self.mic,
            DeviceType::System => &self.system,
        }
    }

    fn slot_mut(&mut self, kind: DeviceType) -> &mut Slot<S> {
        match kind {
            DeviceType::Microphone => &mut self.mic,
            DeviceType::System => &mut self.system,
        }
    }

    fn arrival_failure(kind: DeviceType, stream: &S) -> Option<StreamFailure> {
        stream.is_dead().then(|| StreamFailure { kind, stream_id: stream.stream_id() })
    }

    pub fn epoch(&self, kind: DeviceType) -> u64 {
        self.slot(kind).epoch
    }

    pub fn has_stream(&self, kind: DeviceType) -> bool {
        self.slot(kind).stream.is_some()
    }

    pub fn count(&self) -> usize {
        usize::from(self.mic.stream.is_some()) + usize::from(self.system.stream.is_some())
    }

    /// Start path: place a stream in an (expected empty) slot. Returns what it
    /// replaced, if anything, and the stream's failure if it arrived dead.
    pub fn install(&mut self, kind: DeviceType, stream: S) -> (Option<S>, Option<StreamFailure>) {
        let failure = Self::arrival_failure(kind, &stream);
        let slot = self.slot_mut(kind);
        slot.epoch += 1;
        (slot.stream.replace(stream), failure)
    }

    /// Stop / swap path: empty the slot. Bumps the epoch, which makes any
    /// recovery run that captured the old one obsolete.
    pub fn take(&mut self, kind: DeviceType) -> Option<S> {
        let slot = self.slot_mut(kind);
        slot.epoch += 1;
        slot.stream.take()
    }

    /// Recovery path: take the stream out ONLY if it is still the one that
    /// failed (`stream_id`). Does not bump the epoch; returns the epoch the
    /// run must stay within. A stale event (stream already replaced or gone)
    /// returns `None`.
    pub fn recovery_take(&mut self, kind: DeviceType, stream_id: u64) -> Option<(S, u64)> {
        let slot = self.slot_mut(kind);
        if slot.stream.as_ref().map(Supervised::stream_id) == Some(stream_id) {
            let epoch = slot.epoch;
            slot.stream.take().map(|s| (s, epoch))
        } else {
            None
        }
    }

    /// Recovery path: install the replacement only if nothing else touched the
    /// slot since `recovery_take` (same epoch, still empty). Hands the stream
    /// back otherwise. Does not bump the epoch, so a second failure of the
    /// replacement is handled as a new event.
    pub fn recovery_install(&mut self, kind: DeviceType, epoch: u64, stream: S) -> Result<Option<StreamFailure>, S> {
        let failure = Self::arrival_failure(kind, &stream);
        let slot = self.slot_mut(kind);
        if slot.epoch != epoch || slot.stream.is_some() {
            return Err(stream);
        }
        slot.stream = Some(stream);
        Ok(failure)
    }
}

impl<S: Supervised> Default for StreamSlots<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Fake {
        id: u64,
        dead: Cell<bool>,
    }
    impl Fake {
        fn new(id: u64) -> Self {
            Fake { id, dead: Cell::new(false) }
        }
        fn dead(id: u64) -> Self {
            Fake { id, dead: Cell::new(true) }
        }
    }
    impl Supervised for Fake {
        fn stream_id(&self) -> u64 {
            self.id
        }
        fn is_dead(&self) -> bool {
            self.dead.get()
        }
    }

    const MIC: DeviceType = DeviceType::Microphone;
    const SYS: DeviceType = DeviceType::System;

    /// Finding 1: a mic-swap replacement that dies before it reaches the slot
    /// had its event discarded as stale; the install must report it.
    #[test]
    fn a_stream_installed_dead_by_the_swap_is_reported() {
        let mut s = StreamSlots::new();
        s.install(MIC, Fake::new(1));
        let _old = s.take(MIC); // swap phase 1
        let (replaced, failure) = s.install(MIC, Fake::dead(2)); // swap phase 3
        assert!(replaced.is_none());
        assert_eq!(failure, Some(StreamFailure { kind: MIC, stream_id: 2 }));
    }

    #[test]
    fn a_healthy_install_reports_nothing() {
        let mut s = StreamSlots::new();
        let (_, failure) = s.install(SYS, Fake::new(1));
        assert_eq!(failure, None);
    }

    /// Finding 1 via the recovery path: same single rule, no per-path recheck.
    #[test]
    fn a_stream_recovery_installs_dead_is_reported() {
        let mut s = StreamSlots::new();
        s.install(SYS, Fake::new(1));
        let (_dead, epoch) = s.recovery_take(SYS, 1).unwrap();
        let failure = s.recovery_install(SYS, epoch, Fake::dead(2)).ok().unwrap();
        assert_eq!(failure, Some(StreamFailure { kind: SYS, stream_id: 2 }));
    }

    /// Finding 4: a mic swap that completes while recovery is backing off
    /// makes that recovery obsolete (epoch moved), so it can neither install
    /// over the swapped-in mic nor, via `live`, announce a failure.
    #[test]
    fn a_swap_during_recovery_makes_the_recovery_obsolete() {
        let mut s = StreamSlots::new();
        s.install(MIC, Fake::new(1));
        let (_dead, epoch) = s.recovery_take(MIC, 1).unwrap();
        assert_eq!(s.epoch(MIC), epoch);

        // hot-swap runs: phase 1 (slot already empty) then phase 3
        assert!(s.take(MIC).is_none());
        s.install(MIC, Fake::new(9));

        assert_ne!(s.epoch(MIC), epoch, "swap must invalidate the recovery run");
        let rejected = s.recovery_install(MIC, epoch, Fake::new(2));
        assert!(rejected.is_err(), "recovery must not install over the swap");
        assert_eq!(s.slot(MIC).stream.as_ref().unwrap().id, 9, "swapped-in mic untouched");
    }

    /// Even a swap that only takes (phase 1) and has not installed yet owns the slot.
    #[test]
    fn a_swap_in_flight_blocks_recovery_install() {
        let mut s = StreamSlots::new();
        s.install(MIC, Fake::new(1));
        let (_dead, epoch) = s.recovery_take(MIC, 1).unwrap();
        s.take(MIC);
        assert!(s.recovery_install(MIC, epoch, Fake::new(2)).is_err());
    }

    #[test]
    fn recovery_own_install_keeps_the_epoch_stable() {
        let mut s = StreamSlots::new();
        s.install(SYS, Fake::new(1));
        let (_dead, epoch) = s.recovery_take(SYS, 1).unwrap();
        assert!(s.recovery_install(SYS, epoch, Fake::new(2)).is_ok());
        assert_eq!(s.epoch(SYS), epoch, "recovery installs do not self-invalidate");
    }

    #[test]
    fn a_stale_event_does_not_take_a_healthy_successor() {
        let mut s = StreamSlots::new();
        s.install(MIC, Fake::new(5));
        assert!(s.recovery_take(MIC, 4).is_none(), "id 4 is not in the slot");
        assert!(s.has_stream(MIC));
    }

    #[test]
    fn count_and_slots_are_independent_per_kind() {
        let mut s = StreamSlots::new();
        s.install(MIC, Fake::new(1));
        s.install(SYS, Fake::new(2));
        assert_eq!(s.count(), 2);
        s.take(MIC);
        assert_eq!(s.count(), 1);
        assert!(s.has_stream(SYS));
    }
}
