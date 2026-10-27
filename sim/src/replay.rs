// SPDX-License-Identifier: GPL-2.0-or-later
//! The goal replay: a ring of the last few seconds of the world, a clock that
//! plays them back in slow motion round the moment the ball crosses the line, and
//! the camera that follows.
//!
//! What is recorded is state, not inputs. The sim is deterministic, so inputs
//! and a snapshot would also do, but a replay frame then costs a whole physics
//! tick (and the AI's thinking) where this costs a copy, and a rolling snapshot
//! has to be kept up to date by re-simulating. A frame here is 64 bytes: the
//! ball and both cars to the unit, in the fields the renderer reads. The ring
//! holds 240 frames (four seconds), 15,360 bytes.
//!
//! Playback never touches the live match: it writes into a scratch [`Sim`] that
//! the renderer is handed in place of the real one.

use crate::{uu, Sim, FP, GOAL_FREEZE_TICKS, V3};

/// Frames in the ring: four seconds of ticks.
pub const FRAMES: usize = 240;
/// How many ticks before the ball crosses the line the replay starts, at most.
pub const PRE: i32 = 150;
/// How many ticks after it the replay runs on, at most: the explosion.
pub const POST: i32 = 90;

/// One car, to the unit.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CarFrame {
    pub p: [i16; 3],
    pub up: [i16; 3],
    pub suspension: [i16; 2],
    pub steer: i16,
    pub yaw: u16,
    pub wheel_spin: u16,
    pub demo: u16,
    /// Bit 0 boosting, 1 grounded, 2 supersonic.
    pub flags: u8,
}

/// The world at one tick.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub ball: [i16; 3],
    pub roll: u16,
    pub roll_dir: u16,
    pub cars: [CarFrame; 2],
    /// The goal celebration's countdown, which the explosion is drawn from.
    pub freeze: u8,
}

const _: () = assert!(core::mem::size_of::<Frame>() == 64);

fn q(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

impl CarFrame {
    fn capture(c: &crate::Car) -> Self {
        Self {
            p: [q(c.p.x >> FP), q(c.p.y >> FP), q(c.p.z >> FP)],
            up: [q(c.up.x), q(c.up.y), q(c.up.z)],
            suspension: c.suspension,
            steer: q(c.steer),
            yaw: c.yaw,
            wheel_spin: c.wheel_spin,
            demo: c.demo_timer,
            flags: u8::from(c.boosting) | u8::from(c.grounded) << 1 | u8::from(c.supersonic) << 2,
        }
    }
}

impl Frame {
    /// The world as the sim has it now.
    pub fn capture(s: &Sim) -> Self {
        Self {
            ball: [
                q(s.ball.p.x >> FP),
                q(s.ball.p.y >> FP),
                q(s.ball.p.z >> FP),
            ],
            roll: s.ball.roll,
            roll_dir: s.ball.roll_dir,
            cars: [CarFrame::capture(&s.car), CarFrame::capture(&s.opponent)],
            freeze: s.goal_freeze.min(255) as u8,
        }
    }
}

/// The last [`FRAMES`] ticks.
pub struct Recorder {
    frames: [Frame; FRAMES],
    total: u32,
    cross: Option<u32>,
}

/// Which frames a replay covers, as sequence numbers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: u32,
    pub cross: u32,
    pub end: u32,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub const fn new() -> Self {
        Self {
            frames: [Frame {
                ball: [0; 3],
                roll: 0,
                roll_dir: 0,
                cars: [CarFrame {
                    p: [0; 3],
                    up: [0; 3],
                    suspension: [0; 2],
                    steer: 0,
                    yaw: 0,
                    wheel_spin: 0,
                    demo: 0,
                    flags: 0,
                }; 2],
                freeze: 0,
            }; FRAMES],
            total: 0,
            cross: None,
        }
    }

    /// Forget everything: a new match, or the replay has been seen.
    pub fn clear(&mut self) {
        self.total = 0;
        self.cross = None;
    }

    /// Is this tick worth recording? Everything is, up to [`POST`] ticks into the
    /// goal celebration: past that the ring is kept as it is, so the ticks before
    /// the goal are still in it when the celebration is over.
    pub fn wants(s: &Sim) -> bool {
        s.goal_freeze == 0 || s.goal_freeze + POST as u16 >= GOAL_FREEZE_TICKS
    }

    /// Record this tick.
    pub fn push(&mut self, s: &Sim) {
        // The celebration is over and the world has been put back for the
        // kickoff: that is not part of the goal.
        if s.goal_freeze == 0 && self.cross.is_some() {
            return;
        }
        let at = self.total as usize % FRAMES;
        self.frames[at] = Frame::capture(s);
        // The tick the ball goes in is the one that starts the celebration.
        if s.goal_freeze == GOAL_FREEZE_TICKS && self.cross.is_none() {
            self.cross = Some(self.total);
        }
        self.total += 1;
    }

    /// The oldest sequence number still in the ring.
    pub fn oldest(&self) -> u32 {
        self.total.saturating_sub(FRAMES as u32)
    }

    /// The frame with sequence number `seq`, held to what the ring still has.
    pub fn get(&self, seq: i64) -> &Frame {
        let seq = seq.clamp(self.oldest() as i64, (self.total.max(1) - 1) as i64);
        &self.frames[seq as usize % FRAMES]
    }

    /// The span a replay of the last goal covers, if there was one.
    pub fn window(&self) -> Option<Window> {
        let cross = self.cross?;
        if self.total == 0 || cross < self.oldest() {
            return None;
        }
        Some(Window {
            start: (cross as i64 - PRE as i64).max(self.oldest() as i64) as u32,
            cross,
            end: (cross as i64 + POST as i64).min(self.total as i64 - 1) as u32,
        })
    }
}

/// Where the replay is, in source ticks relative to the crossing, Q8.
#[derive(Copy, Clone, Debug)]
pub struct Clock {
    pub s: i32,
    pub end: i32,
}

/// Source ticks of the clock's position per display tick, Q8: full speed, a
/// ramp down to a third a second and a half before the ball crosses, a third
/// until just after, and a ramp back up.
pub fn rate_q8(s_q8: i32) -> i32 {
    const SLOW: i32 = 85;
    let rel = s_q8 >> 8;
    if rel < -110 {
        256
    } else if rel < -90 {
        256 - (256 - SLOW) * (s_q8 + 110 * 256) / (20 * 256)
    } else if rel < 8 {
        SLOW
    } else if rel < 38 {
        SLOW + (256 - SLOW) * (s_q8 - 8 * 256) / (30 * 256)
    } else {
        256
    }
}

impl Clock {
    /// A clock over `window`.
    pub fn new(window: Window) -> Self {
        Self {
            s: (window.start as i64 - window.cross as i64) as i32 * 256,
            end: (window.end as i64 - window.cross as i64) as i32 * 256,
        }
    }

    /// One display tick. False once the last frame has been reached.
    pub fn step(&mut self) -> bool {
        self.s = (self.s + rate_q8(self.s)).min(self.end);
        self.s < self.end
    }

    /// Is the clock in the slow part, where the music should be ducked?
    pub fn slow(&self) -> bool {
        rate_q8(self.s) < 200
    }
}

fn lerp(a: i32, b: i32, k: i32) -> i32 {
    a + ((b - a) * k >> 8)
}

/// Angles in Q12 turns, the short way round.
fn lerp_angle(a: u16, b: u16, k: i32) -> u16 {
    let d = ((i32::from(b) - i32::from(a) + 2048).rem_euclid(4096)) - 2048;
    (i32::from(a) + (d * k >> 8)).rem_euclid(4096) as u16
}

fn v3(p: [i16; 3]) -> V3 {
    V3::new(
        uu(i32::from(p[0])),
        uu(i32::from(p[1])),
        uu(i32::from(p[2])),
    )
}

/// The replay's position as frames and a Q8 fraction between them.
fn position(w: Window, s_q8: i32) -> (i64, i32) {
    let pos = w.cross as i64 * 256 + s_q8 as i64;
    (pos >> 8, (pos & 255) as i32)
}

/// Write the world at `s_q8` into `sim`, the scratch copy playback draws, which
/// keeps everything else (the pads, the scores) as it was cloned.
pub fn apply(sim: &mut Sim, rec: &Recorder, w: Window, s_q8: i32) {
    let (i, k) = position(w, s_q8);
    let (a, b) = (rec.get(i), rec.get(i + 1));
    let mix = |x: i16, y: i16| lerp(i32::from(x), i32::from(y), k);
    sim.ball.p = V3::new(
        uu(mix(a.ball[0], b.ball[0])),
        uu(mix(a.ball[1], b.ball[1])),
        uu(mix(a.ball[2], b.ball[2])),
    );
    sim.ball.roll = lerp_angle(a.roll, b.roll, k);
    sim.ball.roll_dir = a.roll_dir;
    sim.goal_freeze = u16::from(a.freeze);
    for (car, (ca, cb)) in [&mut sim.car, &mut sim.opponent]
        .into_iter()
        .zip(a.cars.iter().zip(b.cars.iter()))
    {
        car.p = V3::new(
            uu(mix(ca.p[0], cb.p[0])),
            uu(mix(ca.p[1], cb.p[1])),
            uu(mix(ca.p[2], cb.p[2])),
        );
        car.up = V3::new(
            mix(ca.up[0], cb.up[0]),
            mix(ca.up[1], cb.up[1]),
            mix(ca.up[2], cb.up[2]),
        );
        car.yaw = lerp_angle(ca.yaw, cb.yaw, k);
        car.wheel_spin = lerp_angle(ca.wheel_spin, cb.wheel_spin, k);
        car.steer = mix(ca.steer, cb.steer);
        car.suspension = [
            mix(ca.suspension[0], cb.suspension[0]) as i16,
            mix(ca.suspension[1], cb.suspension[1]) as i16,
        ];
        car.demo_timer = ca.demo;
        car.boosting = ca.flags & 1 != 0;
        car.grounded = ca.flags & 2 != 0;
        car.supersonic = ca.flags & 4 != 0;
    }
}

/// Isqrt for the camera, on i64 inputs that fit i32 squares.
fn isqrt(v: i32) -> i32 {
    psx_math::int32::isqrt_i32(v)
}

/// The replay camera: eye and look-at target in the world (Y up), uu.
///
/// It starts behind the scorer, a little above and along the way the car is
/// travelling, then, from a second and three quarters before the ball crosses
/// the line to just over half a second before it, swings to a low spot at the side
/// of the goal and holds there, so the ball comes across the frame and into the
/// net. The target is the ball throughout, which `look_at` aims at exactly.
pub fn camera(
    rec: &Recorder,
    w: Window,
    s_q8: i32,
    scorer: usize,
) -> ((i32, i32, i32), (i32, i32, i32)) {
    let (i, k) = position(w, s_q8);
    let (a, b) = (rec.get(i), rec.get(i + 1));
    let mix = |x: i16, y: i16| lerp(i32::from(x), i32::from(y), k);
    let ball = (
        mix(a.ball[0], b.ball[0]),
        mix(a.ball[1], b.ball[1]),
        mix(a.ball[2], b.ball[2]),
    );
    let car = (
        mix(a.cars[scorer].p[0], b.cars[scorer].p[0]),
        mix(a.cars[scorer].p[1], b.cars[scorer].p[1]),
        mix(a.cars[scorer].p[2], b.cars[scorer].p[2]),
    );
    // The way the car has been going over the last few ticks.
    let (then, now) = (rec.get(i - 4).cars[scorer].p, rec.get(i + 4).cars[scorer].p);
    let (mut dx, mut dz) = (
        i32::from(now[0]) - i32::from(then[0]),
        i32::from(now[2]) - i32::from(then[2]),
    );
    let mut len = isqrt(dx * dx + dz * dz);
    if len < 8 {
        // Nearly still: look along the car's own nose, toward the ball.
        dx = ball.0 - car.0;
        dz = ball.2 - car.2;
        len = isqrt(dx * dx + dz * dz).max(1);
    }
    let behind = (
        (car.0 - dx * 520 / len).clamp(-3900, 3900),
        car.1 + 240,
        (car.2 - dz * 520 / len).clamp(-4900, 4900),
    );
    // The side spot: low, wide of the goal mouth, in front of the line, on the
    // side the ball is not coming from so it crosses the frame.
    let cross_ball = rec.get(w.cross as i64).ball;
    let early = rec.get(w.cross as i64 - 40).ball;
    let goal = if cross_ball[2] >= 0 { 1 } else { -1 };
    let side = if early[0] >= 0 { -1 } else { 1 };
    let spot = (side * 2700, 170, goal * 4000);
    // Smooth from the first position to the second.
    let t = (((s_q8 >> 8) + 105) * 4096 / 70).clamp(0, 4096);
    let w_ = (3 * t * t >> 12) - (2 * t * t >> 12) * t / 4096;
    let e = |x: i32, y: i32| x + ((y - x) * w_ >> 12);
    (
        (
            e(behind.0, spot.0),
            e(behind.1, spot.1),
            e(behind.2, spot.2),
        ),
        ball,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WinCondition;

    fn sim_at(tick: i32, freeze: u16) -> Sim {
        let mut s = Sim::with_win_condition(WinCondition::GoalLimit(5));
        s.ball.p = V3::new(uu(tick - 100), uu(100), uu(tick * 10));
        s.car.p = V3::new(uu(tick), uu(28), uu(tick * 9));
        s.opponent.p = V3::new(uu(-tick), uu(28), uu(-tick * 9));
        s.car.boosting = tick % 2 == 0;
        s.goal_freeze = freeze;
        s
    }

    /// A match of `lead` ticks, a goal, and what the celebration adds.
    fn recorded(lead: i32, tail: i32) -> Recorder {
        let mut rec = Recorder::new();
        for t in 0..lead {
            rec.push(&sim_at(t, 0));
        }
        for n in 0..tail {
            let s = sim_at(lead + n, GOAL_FREEZE_TICKS - n as u16);
            if Recorder::wants(&s) {
                rec.push(&s);
            }
        }
        rec
    }

    #[test]
    fn the_ring_costs_what_the_notes_say() {
        assert_eq!(core::mem::size_of::<Frame>(), 64);
        assert!(core::mem::size_of::<Recorder>() < 24 * 1024);
        assert!(core::mem::size_of::<Recorder>() >= 64 * FRAMES);
    }

    #[test]
    fn the_window_covers_the_run_up_and_the_explosion() {
        let rec = recorded(400, 150);
        let w = rec.window().unwrap();
        assert_eq!(w.cross, 400);
        // The ring is 240 frames: one short of 150 before and 90 after.
        assert_eq!(w.start, rec.oldest());
        assert!(w.start as i32 >= 400 - PRE && 400 - w.start as i32 >= PRE - 2);
        assert_eq!(w.end as i32, 400 + POST);
        // Recording stopped POST ticks into the celebration.
        assert_eq!(rec.total, 400 + POST as u32 + 1);
    }

    #[test]
    fn a_short_match_replays_what_there_is() {
        let rec = recorded(40, 150);
        let w = rec.window().unwrap();
        assert_eq!(w.start, 0);
        assert_eq!(w.cross, 40);
    }

    #[test]
    fn no_goal_no_replay() {
        let rec = recorded(300, 0);
        assert!(rec.window().is_none());
    }

    #[test]
    fn the_clock_runs_one_way_slows_a_second_and_a_half_out_and_ends() {
        let rec = recorded(400, 150);
        let w = rec.window().unwrap();
        let mut c = Clock::new(w);
        let (mut last, mut ticks, mut slow, mut before_line_slow) = (c.s, 0, 0, 0);
        while c.step() {
            assert!(c.s > last, "the clock stopped or ran back at {ticks}");
            assert!(c.s - last <= 256);
            if c.slow() {
                slow += 1;
                if c.s < 0 {
                    before_line_slow += 1;
                }
            }
            last = c.s;
            ticks += 1;
            assert!(ticks < 2000);
        }
        // About 1.5 s of source time before the line at a third of the speed.
        assert!(
            (260..=330).contains(&before_line_slow),
            "{before_line_slow}"
        );
        assert!(slow > before_line_slow);
        assert!((350..=600).contains(&ticks), "a replay of {ticks} ticks");
        assert_eq!(c.s, c.end);
        // A third speed, at the bottom of the ramp.
        assert_eq!(rate_q8(-20 * 256), 85);
        assert_eq!(rate_q8(-200 * 256), 256);
        assert_eq!(rate_q8(100 * 256), 256);
    }

    #[test]
    fn playback_lands_on_the_frames_and_between_them() {
        let rec = recorded(400, 150);
        let w = rec.window().unwrap();
        let mut sim = sim_at(0, 0);
        apply(&mut sim, &rec, w, -10 * 256);
        assert_eq!(sim.ball.p.z >> FP, 3900);
        let mut mid = sim_at(0, 0);
        apply(&mut mid, &rec, w, -10 * 256 + 128);
        let (a, b) = (rec.get(w.cross as i64 - 10), rec.get(w.cross as i64 - 9));
        let want = (i32::from(a.ball[2]) + i32::from(b.ball[2])) / 2;
        assert!(((mid.ball.p.z >> FP) - want).abs() <= 1);
        // Flags come from the earlier frame, and the explosion's countdown too.
        apply(&mut mid, &rec, w, 5 * 256);
        assert!(mid.goal_freeze > 0);
    }

    #[test]
    fn the_camera_stays_in_the_arena_and_on_the_ball() {
        let rec = recorded(400, 150);
        let w = rec.window().unwrap();
        let mut c = Clock::new(w);
        loop {
            let (eye, target) = camera(&rec, w, c.s, 0);
            assert!(eye.0.abs() <= 4096 && eye.2.abs() <= 5120, "{eye:?}");
            assert!((100..=2000).contains(&eye.1), "{eye:?}");
            let mut sim = sim_at(0, 0);
            apply(&mut sim, &rec, w, c.s);
            assert!((target.0 - (sim.ball.p.x >> FP)).abs() <= 1);
            if !c.step() {
                break;
            }
        }
    }
}
