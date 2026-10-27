// SPDX-License-Identifier: GPL-2.0-or-later
//! Hold-to-skip: the ring that fills while X is held, for the goal replay and
//! the fly-over before a kickoff.

/// Ticks X has to be held: a second.
pub const SKIP_TICKS: u8 = 60;
/// What a released ring loses each tick: it empties in a fifth of a second.
const DRAIN: u8 = 5;

/// How far along a hold is.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct SkipHold {
    ticks: u8,
}

impl SkipHold {
    pub const fn new() -> Self {
        Self { ticks: 0 }
    }

    /// One tick. `held` is the button down now, `pressed` that it went down
    /// this tick. A hold starts only on a press, so X that is already down as
    /// the replay opens (the boost button, held through the goal) or still down
    /// from the skip before does not skip what comes next. Returns true on the
    /// tick the ring completes.
    pub fn step(&mut self, held: bool, pressed: bool) -> bool {
        if held {
            if pressed || self.ticks > 0 {
                self.ticks = (self.ticks + 1).min(SKIP_TICKS);
            }
        } else {
            self.ticks = self.ticks.saturating_sub(DRAIN);
        }
        self.ticks >= SKIP_TICKS
    }

    /// Ticks the ring is filled, `0..=SKIP_TICKS`.
    pub const fn ticks(self) -> u8 {
        self.ticks
    }

    /// How full the ring is, Q12.
    pub const fn q12(self) -> i32 {
        self.ticks as i32 * 4096 / SKIP_TICKS as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hold_of_a_second_completes_and_a_tap_does_not() {
        let mut h = SkipHold::new();
        let mut done_at = None;
        for t in 0..80 {
            if h.step(true, t == 0) && done_at.is_none() {
                done_at = Some(t);
            }
        }
        assert_eq!(done_at, Some(SKIP_TICKS as i32 - 1));

        let mut h = SkipHold::new();
        assert!(!h.step(true, true));
        for _ in 0..30 {
            assert!(!h.step(false, false));
        }
        assert_eq!(h.ticks(), 0);
        // A tap of any length short of the hold never completes.
        let mut h = SkipHold::new();
        for t in 0..SKIP_TICKS - 1 {
            assert!(!h.step(true, t == 0));
        }
    }

    #[test]
    fn the_ring_drains_quickly_when_let_go_and_picks_up_where_it_is() {
        let mut h = SkipHold::new();
        for t in 0..40 {
            h.step(true, t == 0);
        }
        assert_eq!(h.ticks(), 40);
        h.step(false, false);
        assert_eq!(h.ticks(), 40 - DRAIN);
        // Pressed again, it carries on from what is left.
        h.step(true, true);
        assert_eq!(h.ticks(), 40 - DRAIN + 1);
        // Drained, a button still down does not restart it.
        let mut h = SkipHold::new();
        assert!(!h.step(true, false));
        assert_eq!(h.ticks(), 0);
    }

    #[test]
    fn q12_is_the_fraction() {
        let mut h = SkipHold::new();
        for t in 0..30 {
            h.step(true, t == 0);
        }
        assert_eq!(h.q12(), 2048);
    }
}
