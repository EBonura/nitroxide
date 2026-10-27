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
    /// Which cars touched the ball this tick: bit 0 blue, bit 1 orange.
    pub touch: u8,
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
            touch: if s.hit > 0 { s.hit_by } else { 0 },
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
                touch: 0,
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

/// What the director has settled about this goal before the replay starts.
#[derive(Copy, Clone, Debug)]
pub struct Direction {
    /// Source ticks, relative to the crossing, of the decisive touch: the last
    /// contact between a car and the ball before the goal. The cut is here.
    pub touch: i32,
    /// The car that made it, 0 blue and 1 orange.
    pub toucher: usize,
    /// Which end the goal is at: +1 for the orange net at +Z, -1 for blue's.
    pub goal: i32,
    /// True for the shot from low behind the goal looking out through the net;
    /// false for the low side angle at the goal line.
    pub behind_goal: bool,
    /// Which side of the car's line the tracking shot stands: +1 or -1.
    pub lane: i32,
    /// Which side of the goal the side angle stands.
    pub side: i32,
}

/// The two ways to shoot the goal itself.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Style {
    /// Low at the goal line, off to one side, looking across the mouth.
    Side,
    /// Low in the net behind the line, looking out at the ball coming in.
    Behind,
}

/// The two goals before this one, newest first, `None` before there are any.
pub type Recent = [Option<Style>; 2];

/// How far a style's camera may be pressed by the walls, the posts and the
/// roof over a replay before it counts as blocked, in units. The shot from
/// the net sits about 250 off its wish all through, against the back wall
/// of the net, and is meant to.
const MAX_STRAIN: i32 = 400;

/// A car this close to the camera, on the ground plane, is in the way of it.
const CAR_CLEARANCE: i32 = 450;

/// Choose the goal shot: one of the two at random from `roll`, never the same
/// one three goals running, and the other one when this goal would press the
/// chosen camera hard against the walls or put a car in front of its lens.
/// Safety outranks the variety: a style that is blocked is not used even to
/// avoid a repeat.
pub fn pick_style(rec: &Recorder, w: Window, roll: u32, recent: Recent) -> Style {
    let other = |s: Style| match s {
        Style::Side => Style::Behind,
        Style::Behind => Style::Side,
    };
    let mut want = if (roll >> 11) & 1 == 0 {
        Style::Side
    } else {
        Style::Behind
    };
    if recent[0] == Some(want) && recent[1] == Some(want) {
        want = other(want);
    }
    if workable(rec, w, want) || !workable(rec, w, other(want)) {
        want
    } else {
        other(want)
    }
}

/// Whether `style` gives a clear camera for this goal: pushed no more than
/// [`MAX_STRAIN`] off where it wants to be at any point of the replay, and with
/// no car driven or flung up to the lens.
fn workable(rec: &Recorder, w: Window, style: Style) -> bool {
    let d = Direction::new(rec, w, style);
    let mut s = d.touch.max(-90) * 256;
    while s <= (RAMP_OUT + 8) * 256 {
        let (shot, wanted) = goal_shot(rec, w, &d, s);
        let (a, b) = (shot.eye, wanted);
        let strain = isqrt(((a.0 - b.0).pow(2) + (a.1 - b.1).pow(2) + (a.2 - b.2).pow(2)) / 16) * 4;
        if strain > MAX_STRAIN {
            return false;
        }
        let pos = w.cross as i64 * 256 + s as i64;
        for seat in 0..2 {
            let c = car_at(rec, w, seat, pos);
            let (dx, dz) = (c.0 - a.0, c.2 - a.2);
            if isqrt((dx * dx + dz * dz) / 16) * 4 < CAR_CLEARANCE && (c.1 - a.1).abs() < 500 {
                return false;
            }
        }
        s += 6 * 256;
    }
    true
}

/// The blast takes this long to ease out, source ticks after the crossing.
const RAMP_OUT: i32 = 38;

fn unit2(dx: i32, dz: i32) -> (i32, i32, i32) {
    let len = isqrt(dx * dx + dz * dz).max(1);
    (dx * 4096 / len, dz * 4096 / len, len)
}

/// `v` limited to `limit` without a corner: unchanged up to `margin` short of
/// it, then pressed up against it ever more gently, never past.
fn soft_max(v: i32, limit: i32, margin: i32) -> i32 {
    let start = limit - margin;
    if v <= start {
        v
    } else {
        let over = v - start;
        start + margin * over / (margin + over)
    }
}

fn soft_clamp(v: i32, lo: i32, hi: i32, margin: i32) -> i32 {
    -soft_max(-soft_max(v, hi, margin), -lo, margin)
}

/// Where the camera may stand: inside the arena, well off the walls and floor
/// and ceiling, and behind the goal line only between the posts, under the bar
/// and short of the back net. Pushed to the edge smoothly, not cut off.
fn keep_inside(e: (i32, i32, i32), goal_box: bool) -> (i32, i32, i32) {
    let wall = crate::HALF_X - 320;
    let x = soft_clamp(e.0, -wall, wall, 500);
    // The room behind the line: the whole depth of the net in the middle of the
    // mouth, none at all beyond the posts, a smooth ramp between.
    let mouth = crate::GOAL_HALF_W - 150;
    let ramp = 320;
    let ax = x.abs();
    let base_end = crate::HALF_Z - 330;
    let full_end = crate::HALF_Z + crate::GOAL_DEPTH - 130;
    let end = if !goal_box || ax >= mouth + ramp {
        base_end
    } else if ax <= mouth {
        full_end
    } else {
        base_end + (full_end - base_end) * (mouth + ramp - ax) / ramp
    };
    let reach = end - base_end;
    let z = soft_clamp(e.2, -end, end, 500);
    let y = soft_clamp(e.1, 95, crate::CEIL - 400, 300);
    (
        x,
        if goal_box && reach > 0 {
            y.min(crate::GOAL_H - 110)
        } else {
            y
        },
        z,
    )
}

fn mix3(a: (i32, i32, i32), b: (i32, i32, i32), k: i32) -> (i32, i32, i32) {
    (lerp(a.0, b.0, k), lerp(a.1, b.1, k), lerp(a.2, b.2, k))
}

fn ease(t: i32) -> i32 {
    let t = t.clamp(0, 256);
    (3 * t * t >> 8) - (2 * t * t >> 8) * t / 256
}

impl Direction {
    /// Read the recording and decide the shots, the goal shot in `style`.
    pub fn new(rec: &Recorder, w: Window, style: Style) -> Self {
        let cross = w.cross as i64;
        let at_cross = rec.get(cross);
        let goal = if at_cross.ball[2] >= 0 { 1 } else { -1 };
        // The decisive touch: the last frame before the goal with a contact.
        let mut found = None;
        let mut seq = cross;
        while seq >= w.start as i64 {
            let t = rec.get(seq).touch;
            if t != 0 {
                found = Some((seq, t));
                break;
            }
            seq -= 1;
        }
        let (touch, toucher) = match found {
            Some((seq, bits)) => ((seq - cross) as i32, usize::from(bits & 1 == 0)),
            None => {
                // No touch in the ring: a long ball, or a roll from the kick.
                // Cut as the slow motion starts, on the car nearest the ball.
                let f = rec.get(cross - 90);
                let d = |c: usize| {
                    let (dx, dz) = (
                        i32::from(f.cars[c].p[0]) - i32::from(f.ball[0]),
                        i32::from(f.cars[c].p[2]) - i32::from(f.ball[2]),
                    );
                    dx * dx / 16 + dz * dz / 16
                };
                (-90, usize::from(d(1) < d(0)))
            }
        };
        // Which way the ball came in, over its last half second, for the side
        // the side angle stands on.
        let before = rec.get(cross - 12).ball;
        let side = if before[0] >= 0 { -1 } else { 1 };
        // The tracking shot stands on the side of the car's line that faces the
        // middle of the pitch, so it has room.
        let tf = rec.get(cross + touch as i64);
        let (tx, tz, _) = unit2(
            i32::from(rec.get(cross + touch as i64 + 8).cars[toucher].p[0])
                - i32::from(rec.get(cross + touch as i64 - 8).cars[toucher].p[0]),
            i32::from(rec.get(cross + touch as i64 + 8).cars[toucher].p[2])
                - i32::from(rec.get(cross + touch as i64 - 8).cars[toucher].p[2]),
        );
        let (px, _pz) = (-tz, tx);
        let lane = if (px * i32::from(tf.cars[toucher].p[0])) > 0 {
            -1
        } else {
            1
        };
        Self {
            touch,
            toucher,
            goal,
            behind_goal: style == Style::Behind,
            lane,
            side,
        }
    }
}

/// One frame's direction: where the camera stands, what it looks at and how
/// long its lens is.
#[derive(Copy, Clone, Debug)]
pub struct Shot {
    pub eye: (i32, i32, i32),
    pub target: (i32, i32, i32),
    /// The projection plane in pixels: a bigger one is a longer lens.
    pub plane: i32,
    /// True from the cut on the decisive touch.
    pub cut: bool,
}

fn car_at(rec: &Recorder, w: Window, seat: usize, pos_q8: i64) -> (i32, i32, i32) {
    let (i, k) = (pos_q8 >> 8, (pos_q8 & 255) as i32);
    let (a, b) = (rec.get(i), rec.get(i + 1));
    let m = |x: i16, y: i16| lerp(i32::from(x), i32::from(y), k);
    let _ = w;
    (
        m(a.cars[seat].p[0], b.cars[seat].p[0]),
        m(a.cars[seat].p[1], b.cars[seat].p[1]),
        m(a.cars[seat].p[2], b.cars[seat].p[2]),
    )
}

fn ball_at(rec: &Recorder, pos_q8: i64) -> (i32, i32, i32) {
    let (i, k) = (pos_q8 >> 8, (pos_q8 & 255) as i32);
    let (a, b) = (rec.get(i), rec.get(i + 1));
    let m = |x: i16, y: i16| lerp(i32::from(x), i32::from(y), k);
    (
        m(a.ball[0], b.ball[0]),
        m(a.ball[1], b.ball[1]),
        m(a.ball[2], b.ball[2]),
    )
}

/// A car's position smoothed over `half` ticks either side: a dolly that does
/// not follow every wobble of the car.
fn smooth_car(rec: &Recorder, w: Window, seat: usize, pos_q8: i64, half: i64) -> (i32, i32, i32) {
    let (mut x, mut y, mut z, mut n) = (0, 0, 0, 0);
    let mut o = -half;
    while o <= half {
        let p = car_at(rec, w, seat, pos_q8 + o * 256);
        x += p.0;
        y += p.1;
        z += p.2;
        n += 1;
        o += half / 3;
    }
    (x / n, y / n, z / n)
}

/// Direct the replay at `s_q8` (source ticks relative to the crossing, Q8):
/// a tracking shot beside the scorer, a cut on the decisive touch into a low
/// angle at the goal that pushes in through the slow motion, and a pull-back as
/// the blast eases out. All of it a function of the recording and the clock,
/// moves eased, nothing stepped but the one cut.
pub fn direct(rec: &Recorder, w: Window, d: &Direction, s_q8: i32) -> Shot {
    let pos = w.cross as i64 * 256 + s_q8 as i64;
    let rel = s_q8 >> 8;
    let ball = ball_at(rec, pos);
    if rel < d.touch {
        // The tracking shot: low, beside the car, a little ahead of it, on a long
        // lens, looking between the car and the ball.
        let car = smooth_car(rec, w, d.toucher, pos, 12);
        let (a, b) = (
            smooth_car(rec, w, d.toucher, pos + 10 * 256, 8),
            smooth_car(rec, w, d.toucher, pos - 10 * 256, 8),
        );
        let (tx, tz, len) = unit2(a.0 - b.0, a.2 - b.2);
        // Standing still, face the ball instead.
        let (tx, tz) = if len < 40 {
            let (bx, bz, _) = unit2(ball.0 - car.0, ball.2 - car.2);
            (bx, bz)
        } else {
            (tx, tz)
        };
        let (px, pz) = (-tz * d.lane, tx * d.lane);
        let eye = (
            car.0 + (px * 900 >> 12) + (tx * 300 >> 12),
            150 + car.1 / 3,
            car.2 + (pz * 900 >> 12) + (tz * 300 >> 12),
        );
        let target = (
            lerp(car.0, ball.0, 110),
            lerp(car.1, ball.1, 110) + 30,
            lerp(car.2, ball.2, 110),
        );
        return Shot {
            eye: keep_inside(eye, false),
            target,
            plane: 340,
            cut: false,
        };
    }
    goal_shot(rec, w, d, s_q8).0
}

/// The goal shot, from the touch on: it pushes in over the slow motion and
/// pulls back as the blast eases out. Returns the shot and the eye it wanted
/// before the arena pushed it about.
fn goal_shot(rec: &Recorder, w: Window, d: &Direction, s_q8: i32) -> (Shot, (i32, i32, i32)) {
    let pos = w.cross as i64 * 256 + s_q8 as i64;
    let rel = s_q8 >> 8;
    let ball = ball_at(rec, pos);
    let t0 = d.touch.max(-90);
    let into = ease((((s_q8 - t0 * 256) as i64 * 256) / ((8 - t0).max(1) as i64 * 256)) as i32);
    let out = ease(((s_q8 - 8 * 256) as i64 * 256 / (RAMP_OUT as i64 * 256 - 8 * 256)) as i32);
    // The ball with room to move into: the aim runs ahead of it along its path
    // until it is in the net.
    let (v0, v1) = (ball_at(rec, pos - 3 * 256), ball_at(rec, pos + 3 * 256));
    let (vx, vz, vlen) = unit2(v1.0 - v0.0, v1.2 - v0.2);
    let lead = if vlen > 6 { 230 * (256 - out) / 256 } else { 0 };
    let target = (
        ball.0 + (vx * lead >> 12),
        ball.1 + 24,
        ball.2 + (vz * lead >> 12),
    );
    let at_cross = rec.get(w.cross as i64).ball;
    let (eye0, plane0, plane1, box_) = if d.behind_goal {
        (
            (
                i32::from(at_cross[0]) / 3,
                200,
                d.goal * (crate::HALF_Z + crate::GOAL_DEPTH - 150),
            ),
            210,
            250,
            true,
        )
    } else {
        (
            (d.side * 2750, 160, d.goal * (crate::HALF_Z - 560)),
            300,
            360,
            false,
        )
    };
    // Push in toward the ball, and out again after the goal.
    let eye = mix3(eye0, target, 22 * into / 256);
    let eye = mix3(
        eye,
        (2 * eye0.0 - target.0, eye0.1 + 40, 2 * eye0.2 - target.2),
        55 * out / 256,
    );
    let plane = lerp(plane0, plane1, into) - (plane1 - plane0 + 30) * out / 256;
    (
        Shot {
            eye: keep_inside(eye, box_),
            target,
            plane,
            cut: rel == d.touch,
        },
        eye,
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

    /// A recording of a goal made by hand: `ball(t)` and `car(t)` give the bodies
    /// at tick `t` (the goal is scored at `lead`), with a touch at each tick in
    /// `touches` (by the blue car).
    fn staged(
        lead: i32,
        ball: impl Fn(i32) -> (i32, i32, i32),
        car: impl Fn(i32) -> (i32, i32, i32),
        touches: &[i32],
    ) -> Recorder {
        let mut rec = Recorder::new();
        let mut s = Sim::with_win_condition(WinCondition::GoalLimit(9));
        for t in 0..lead + 100 {
            let b = ball(t.min(lead + 1));
            s.ball.p = V3::new(uu(b.0), uu(b.1), uu(b.2));
            let c = car(t);
            s.car.p = V3::new(uu(c.0), uu(c.1), uu(c.2));
            s.opponent.p = V3::new(uu(-1500), uu(28), uu(-3000));
            s.hit = if touches.contains(&t) { 600 } else { 0 };
            s.hit_by = if touches.contains(&t) { 1 } else { 0 };
            s.goal_freeze = if t >= lead {
                GOAL_FREEZE_TICKS - (t - lead) as u16
            } else {
                0
            };
            if Recorder::wants(&s) {
                rec.push(&s);
            }
        }
        rec
    }

    /// Walk a whole replay and hold the director to its rules.
    fn judge(rec: &Recorder, style: Style) -> Direction {
        let w = rec.window().unwrap();
        let d = Direction::new(rec, w, style);
        let mut c = Clock::new(w);
        let (mut last, mut cuts, mut n) = (None::<Shot>, 0, 0);
        loop {
            let shot = direct(rec, w, &d, c.s);
            let (e, t) = (shot.eye, shot.target);
            assert!(e.0.abs() <= crate::HALF_X - 300, "{e:?} at {n}");
            assert!((90..=crate::CEIL).contains(&e.1), "{e:?} at {n}");
            let in_net = e.2.abs() > crate::HALF_Z - 300;
            assert!(e.2.abs() <= crate::HALF_Z + crate::GOAL_DEPTH, "{e:?}");
            if in_net {
                assert!(
                    e.0.abs() <= crate::GOAL_HALF_W && e.1 <= crate::GOAL_H,
                    "{e:?} behind the line"
                );
            }
            let dist =
                isqrt((e.0 - t.0).pow(2) / 16 + (e.1 - t.1).pow(2) / 16 + (e.2 - t.2).pow(2) / 16)
                    * 4;
            assert!(dist >= 180, "the eye is on the ball at {n}: {dist}");
            assert!((180..=430).contains(&shot.plane), "{}", shot.plane);
            let (pos, _) = (w.cross as i64 * 256 + c.s as i64, 0);
            let b = ball_at(rec, pos);
            if (c.s >> 8) >= d.touch {
                assert!(
                    (t.0 - b.0).abs() <= 260
                        && (t.2 - b.2).abs() <= 260
                        && (t.1 - b.1).abs() <= 400
                );
            }
            if let Some(l) = last {
                let step = isqrt(
                    ((e.0 - l.eye.0).pow(2) + (e.1 - l.eye.1).pow(2) + (e.2 - l.eye.2).pow(2)) / 16,
                ) * 4;
                if step > 150 {
                    cuts += 1;
                    assert!(
                        c.s >> 8 == d.touch || (c.s >> 8) == d.touch + 1,
                        "a cut off the beat at {}",
                        c.s >> 8
                    );
                }
                assert!(
                    (shot.plane - l.plane).abs() <= 14 || step > 150,
                    "plane jump off a cut"
                );
            }
            last = Some(shot);
            n += 1;
            if !c.step() {
                break;
            }
        }
        assert!(cuts <= 1, "{cuts} cuts");
        d
    }

    /// Both styles hold the rules on one goal.
    fn judge_both(rec: &Recorder) -> Direction {
        judge(rec, Style::Behind);
        judge(rec, Style::Side)
    }

    #[test]
    fn a_driven_goal_is_cut_on_the_touch_and_shot_from_the_side_at_the_line() {
        // A car running down the middle hits the ball at tick 150; it rolls in.
        let rec = staged(
            230,
            |t| (0, 92, if t < 150 { 3000 } else { 3000 + (t - 150) * 27 }),
            |t| (0, 28, 1500 + t * 10),
            &[149, 150, 151],
        );
        let d = judge_both(&rec);
        assert_eq!(d.toucher, 0);
        assert_eq!(d.touch, 151 - 230);
        assert_eq!(d.goal, 1);
        assert!(!d.behind_goal);
    }

    #[test]
    fn a_ball_coming_in_across_the_mouth_is_shot_from_the_side() {
        let rec = staged(
            200,
            |t| (2400 - t.min(200) * 10, 100, 4000 + t.min(200) * 6),
            |t| (2600 - t * 10, 28, 3600 + t * 6),
            &[100],
        );
        let d = judge_both(&rec);
        assert!(!d.behind_goal);
    }

    #[test]
    fn a_long_shot_with_no_touch_in_the_ring_cuts_as_the_slow_motion_starts() {
        let rec = staged(
            300,
            |t| (0, 92, -2000 + t * 24),
            |t| (0, 28, -2800 + t * 12),
            &[],
        );
        let d = judge_both(&rec);
        assert_eq!(d.touch, -90);
    }

    #[test]
    fn an_aerial_and_an_own_goal_hold_the_same_rules() {
        // High in the air, in at the bar.
        let rec = staged(
            200,
            |t| (300, 600, 3600 + t.min(200) * 7),
            |t| (300, 520, 3500 + t * 7),
            &[120],
        );
        assert!(judge(&rec, Style::Behind).behind_goal);
        judge(&rec, Style::Side);
        // Into the blue net, at -Z.
        let rec = staged(
            220,
            |t| (-200, 92, -3200 - t.min(220) * 9),
            |t| (-200, 28, -3100 - t * 9),
            &[130],
        );
        let d = judge_both(&rec);
        assert_eq!(d.goal, -1);
    }

    #[test]
    fn the_style_is_random_never_thrice_and_falls_back_when_blocked() {
        // A high ball in the middle: both styles work.
        let high = staged(
            200,
            |t| (300, 600, 3600 + t.min(200) * 7),
            |t| (300, 520, 3500 + t * 7),
            &[120],
        );
        let w = high.window().unwrap();
        assert!(workable(&high, w, Style::Behind) && workable(&high, w, Style::Side));
        let mut seen = [0; 2];
        for roll in 0..64u32 {
            let s = pick_style(&high, w, roll.wrapping_mul(0x9e37_79b9), [None, None]);
            seen[usize::from(s == Style::Behind)] += 1;
        }
        assert!(seen[0] > 16 && seen[1] > 16, "{seen:?}");
        // Never the same style a third time running, whatever the roll says.
        for roll in 0..64u32 {
            let roll = roll.wrapping_mul(0x9e37_79b9);
            let first = pick_style(&high, w, roll, [None, None]);
            let again = pick_style(&high, w, roll, [Some(first), Some(first)]);
            assert_ne!(again, first);
            assert_eq!(pick_style(&high, w, roll, [Some(first), None]), first);
        }
        // A car that drives on into the net is in the way of a camera there:
        // the side angle, even when the roll and the history ask for the other.
        let rolled_in = staged(
            230,
            |t| (0, 92, if t < 150 { 3000 } else { 3000 + (t - 150) * 27 }),
            |t| (0, 28, (2000 + t * 14).min(5800)),
            &[149, 150, 151],
        );
        let w = rolled_in.window().unwrap();
        assert!(!workable(&rolled_in, w, Style::Behind) && workable(&rolled_in, w, Style::Side));
        for roll in 0..64u32 {
            let roll = roll.wrapping_mul(0x9e37_79b9);
            assert_eq!(
                pick_style(&rolled_in, w, roll, [Some(Style::Side), Some(Style::Side)]),
                Style::Side
            );
            assert_eq!(pick_style(&rolled_in, w, roll, [None, None]), Style::Side);
        }
        // A ball on the floor is shot either way.
        let low = staged(
            230,
            |t| (0, 92, if t < 150 { 3000 } else { 3000 + (t - 150) * 27 }),
            |t| (0, 28, 1500 + t * 10),
            &[149, 150, 151],
        );
        let w = low.window().unwrap();
        assert!(workable(&low, w, Style::Behind) && workable(&low, w, Style::Side));
    }

    #[test]
    fn the_director_roll_is_the_match_and_nothing_else() {
        let a = sim_at(300, 0);
        let b = sim_at(300, 0);
        assert_eq!(a.director_roll(), b.director_roll());
        assert_eq!(a.director_roll(), a.director_roll());
        assert_ne!(a.director_roll(), sim_at(301, 0).director_roll());
        let mut c = sim_at(300, 0);
        c.score_blue += 1;
        assert_ne!(a.director_roll(), c.director_roll());
    }

    #[test]
    fn the_camera_is_pushed_off_walls_without_a_corner() {
        let step = |v: i32| keep_inside((v, 400, 0), false).0;
        assert_eq!(step(0), 0);
        let mut last = step(2000);
        for v in 2000..6000 {
            let now = step(v);
            assert!(now >= last && now - last <= 1 && now <= crate::HALF_X - 320);
            last = now;
        }
    }
}
