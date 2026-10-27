// SPDX-License-Identifier: GPL-2.0-or-later
//! What a match adds up to for the results screen: shots, saves, boost burned,
//! the top speed and the time spent off the ground, for each side.
//!
//! Kept by the sim as it plays, in integers, so the numbers are the same on
//! every run of a match and can be tested on the host.

use crate::{uu, Sim, Team, GOAL_HALF_W, HALF_Z};

/// One side's tally.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TeamStats {
    /// Touches that sent the ball toward the other net, goals included.
    pub shots: u16,
    /// Shots at this side's net turned away by one of its cars.
    pub saves: u16,
    /// Boost burned, in 1/64 pips (`BOOST_SCALE` of them make a pip).
    pub boost_used: u32,
    /// Fastest the car went, in sub-units a tick (the unit of `Car::v`).
    pub top_speed: u16,
    /// Ticks off the ground.
    pub air_ticks: u32,
}

impl TeamStats {
    /// Boost burned, in whole pips of a hundred-pip tank.
    pub const fn boost_pips(&self) -> u32 {
        self.boost_used / crate::BOOST_SCALE as u32
    }

    /// Top speed in km/h as Rocket League's own gauge reads it: 1 uu is a
    /// centimetre, so a unit a second is 0.036 km/h.
    pub const fn top_kph(&self) -> u32 {
        self.top_speed as u32 * 60 / 64 * 36 / 1000
    }

    /// Time in the air, in whole seconds.
    pub const fn air_secs(&self) -> u32 {
        self.air_ticks / 60
    }
}

/// Both sides, blue first, and what is needed to tell a save from a clearance.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchStats {
    pub team: [TeamStats; 2],
    /// The side a shot on goal is live for, until it scores, is turned away or
    /// runs out. 0 none, 1 blue is shooting, 2 orange.
    live_shot: u8,
    /// Ticks the live shot has been in the air.
    live_age: u16,
    /// Who touched the ball last: 0 nobody, 1 blue, 2 orange.
    last_touch: u8,
}

/// A shot has this many ticks to arrive or it is a clearance.
const SHOT_LIFE: u16 = 300;
/// The ball has to be heading for the line at this many sub-units a tick, about
/// 700 uu/s: a dribble toward the net is not a shot.
const SHOT_SPEED: i32 = 750;
/// The net, widened by this much each side, is what a shot is aimed at: a shot
/// that goes just wide is still a shot.
const SHOT_MARGIN: i32 = 350;

impl Sim {
    /// Count one tick. `boosts` are the tanks before the tick ran.
    pub(crate) fn tally(&mut self, boosts: [i32; 2]) {
        let cars = [(&self.car, boosts[0]), (&self.opponent, boosts[1])];
        for (i, (car, before)) in cars.into_iter().enumerate() {
            let t = &mut self.stats.team[i];
            if car.boosting {
                t.boost_used += (before - car.boost).max(0) as u32;
            }
            if !car.wrecked() {
                if !car.grounded {
                    t.air_ticks += 1;
                }
                t.top_speed = t.top_speed.max(car.v.len().min(i32::from(u16::MAX)) as u16);
            }
        }
        // A live shot ages out, or has been turned back toward the middle.
        if self.stats.live_shot != 0 {
            self.stats.live_age += 1;
            if self.stats.live_age > SHOT_LIFE {
                self.stats.live_shot = 0;
            }
        }
        for (i, bit) in [1u8, 2u8].into_iter().enumerate() {
            if self.hit_by & bit != 0 {
                self.touch(i);
            }
        }
    }

    /// Car `seat` has just touched the ball.
    fn touch(&mut self, seat: usize) {
        let me = seat as u8 + 1;
        self.stats.last_touch = me;
        // Another car's touch on a live shot at its net is a save.
        if self.stats.live_shot != 0 && self.stats.live_shot != me {
            self.stats.team[seat].saves += 1;
            self.stats.live_shot = 0;
        }
        // Blue shoots at +Z, orange at -Z.
        let dir = if seat == 0 { 1 } else { -1 };
        let (p, v) = (self.ball.p, self.ball.v);
        let toward = v.z * dir;
        if toward < SHOT_SPEED {
            return;
        }
        let line = dir * uu(HALF_Z);
        let ticks = ((line - p.z) * dir) / toward;
        if !(0..=SHOT_LIFE as i32).contains(&ticks) {
            return;
        }
        // Where it crosses the line, in sub-units: the product is kept in range
        // by the cap on the ticks.
        let x = p.x + v.x * ticks;
        if x.abs() <= uu(GOAL_HALF_W + SHOT_MARGIN) {
            self.stats.team[seat].shots += 1;
            self.stats.live_shot = me;
            self.stats.live_age = 0;
        }
    }

    /// A goal for `team`: a shot, if it was not counted already and the scorer
    /// touched the ball last.
    pub(crate) fn tally_goal(&mut self, team: Team) {
        let me = match team {
            Team::Blue => 1,
            Team::Orange => 2,
        };
        if self.stats.last_touch == me && self.stats.live_shot != me {
            self.stats.team[me as usize - 1].shots += 1;
        }
        self.stats.live_shot = 0;
        self.stats.last_touch = 0;
    }

    /// A kickoff clears what a shot was in flight.
    pub(crate) fn tally_kickoff(&mut self) {
        self.stats.live_shot = 0;
        self.stats.last_touch = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim() -> Sim {
        Sim::with_win_condition(crate::WinCondition::GoalLimit(5))
    }

    #[test]
    fn a_hard_touch_at_the_net_is_a_shot_and_the_other_car_turning_it_is_a_save() {
        let mut s = sim();
        s.ball.p = crate::V3::new(0, uu(100), uu(3000));
        s.ball.v = crate::V3::new(0, 0, uu(40));
        s.touch(0);
        assert_eq!(s.stats.team[0].shots, 1);
        // The orange car gets to it.
        s.touch(1);
        assert_eq!(s.stats.team[1].saves, 1);
        assert_eq!(s.stats.live_shot, 0);
    }

    #[test]
    fn a_dribble_or_a_wide_ball_is_not_a_shot() {
        let mut s = sim();
        s.ball.p = crate::V3::new(0, uu(100), uu(0));
        s.ball.v = crate::V3::new(0, 0, uu(5));
        s.touch(0);
        s.ball.v = crate::V3::new(uu(30), 0, uu(30));
        s.ball.p = crate::V3::new(uu(2000), uu(100), uu(3000));
        s.touch(0);
        // Away from the net it is attacking.
        s.ball.v = crate::V3::new(0, 0, -uu(40));
        s.touch(0);
        assert_eq!(s.stats.team[0].shots, 0);
    }

    #[test]
    fn a_goal_the_scorer_touched_is_a_shot_once() {
        let mut s = sim();
        s.stats.last_touch = 1;
        s.tally_goal(Team::Blue);
        assert_eq!(s.stats.team[0].shots, 1);
        // An own goal is not the scorer's shot.
        s.stats.last_touch = 2;
        s.tally_goal(Team::Blue);
        assert_eq!(s.stats.team[0].shots, 1);
        // Already counted when it left the boot.
        s.stats.last_touch = 2;
        s.stats.live_shot = 2;
        s.tally_goal(Team::Orange);
        assert_eq!(s.stats.team[1].shots, 0);
    }

    #[test]
    fn a_played_match_adds_up() {
        let mut s = sim();
        let input = crate::Input {
            throttle: 127,
            boost: true,
            ..crate::Input::default()
        };
        for _ in 0..600 {
            s.tick(&input);
        }
        let t = s.stats.team[0];
        assert!(t.boost_used > 0 && t.boost_pips() > 0);
        assert!(t.top_speed > 500, "{}", t.top_speed);
        assert!(t.top_kph() > 20);
    }
}
