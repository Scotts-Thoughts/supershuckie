//! Replays played side by side, kept on the same timer: the comparison side.
//!
//! A comparison replay's core is not paced by a clock of its own. Whoever drives it (the
//! frontend, from the main replay's position) publishes a target replay time into a
//! [`ReplaySyncTarget`]; the core thread runs the replay until it reaches that time, seeks when
//! the target jumps (backwards, or far enough ahead that a keyframe saves emulating the way
//! there), and parks while it is there. Pausing, stopping, seeking and speed changes of whatever
//! drives the target therefore carry over by themselves. See
//! [`ThreadedSuperShuckieCore::set_replay_sync_target`](crate::ThreadedSuperShuckieCore::set_replay_sync_target).

use std::sync::atomic::{AtomicI64, Ordering};

/// The replay time (in the replay's own milliseconds, as its frames' timestamps count them) a
/// synced core is to show. Shared between the thread setting it and the core thread following
/// it; reading and writing never block.
#[derive(Debug)]
pub struct ReplaySyncTarget {
    millis: AtomicI64
}

impl ReplaySyncTarget {
    /// Stored while there is no target: the core holds where it is.
    const HOLD: i64 = i64::MIN;

    /// A target that holds until the first [`Self::set`].
    pub fn new() -> Self {
        Self { millis: AtomicI64::new(Self::HOLD) }
    }

    /// Show the first frame at or after replay time `millis` (which may lie before the replay
    /// starts or after it ends: the core then sits on its first or last frame). Wake the core
    /// thread afterwards (see [`ThreadedSuperShuckieCore::wake`](crate::ThreadedSuperShuckieCore::wake))
    /// so that it does not wait out its park.
    #[inline]
    pub fn set(&self, millis: i64) {
        self.millis.store(millis.max(Self::HOLD + 1), Ordering::Relaxed);
    }

    /// Stop following: the core stays on the frame it is on.
    #[inline]
    pub fn hold(&self) {
        self.millis.store(Self::HOLD, Ordering::Relaxed);
    }

    /// The current target, `None` while holding.
    #[inline]
    pub fn get(&self) -> Option<i64> {
        let millis = self.millis.load(Ordering::Relaxed);
        (millis != Self::HOLD).then_some(millis)
    }
}

impl Default for ReplaySyncTarget {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_until_set() {
        let target = ReplaySyncTarget::new();
        assert_eq!(target.get(), None);
        target.set(-5_000);
        assert_eq!(target.get(), Some(-5_000));
        target.set(i64::MIN);
        assert_eq!(target.get(), Some(i64::MIN + 1));
        target.hold();
        assert_eq!(target.get(), None);
    }
}
