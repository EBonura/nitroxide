// SPDX-License-Identifier: GPL-2.0-or-later
//! The chase camera, as pure integer maths so it runs (and is tested) on the
//! host. `game/src/draw.rs` owns the screen: it turns the [`Pose`] this
//! returns into a view matrix and nothing else.
//!
//! # Model
//!
//! A spring arm around the car, written in cylindrical form: the eye sits at
//! `azimuth`, `length` (horizontal) and `rise` (vertical) from the car. The
//! three are rate limited separately, so a change of side is an orbit at a
//! steady radius and can never pass through the car, which the old
//! Cartesian-offset limiter did whenever the wanted side flipped.
//!
//! * **Camera up.** The arm's height and its push out from a wall follow a
//!   smoothed `up` vector, not the car's attitude. On a surface it chases the
//!   surface normal; in the air it levels to world up, so flips and air rolls
//!   turn the car under a steady camera instead of swinging the eye around it.
//! * **Heading.** On the ground the arm trails the car's nose (or the car to
//!   ball line in ball cam). In the air it trails the horizontal velocity,
//!   falling back to the nose at low speed.
//! * **Walls.** Each frame the arm is cast against the arena (side walls, end
//!   walls, corner chamfers, rounded joints). Where the wanted direction would
//!   leave less than the wanted length, the azimuth is moved to the nearest
//!   edge of the blocked arc, keeping the side it last used unless the other
//!   edge is much nearer, and the length is clamped by the ray. A minimum arm
//!   keeps the eye out of the car.
//! * **Framing.** Ball cam places the view pitch and yaw so the car sits in the
//!   lower third of the screen and the ball stays inside the top margin, with
//!   the car winning when both cannot fit.
//!
//! Lengths are uu, angles Q12 turns, and the world is Y up. Everything is
//! `i32`: the console has no FPU.

use crate::angle::atan2_q12_fine as atan2_fine;
use crate::{Car, Sim, V3, CAR_HALF_H, CEIL, CORNER, CORNER_JOINT_PLANES, FP, HALF_X, HALF_Z};
use psx_math::int32::isqrt_i32;
use psx_math::sincos::{atan2_q12, cos_q12, sin_q12};

/// The tunable shape of the rig. `Rig::DEFAULT` is the shipping camera;
/// `Rig::WHEELS` is the close three-quarter inspection view `boot-wheels`
/// builds use.
#[derive(Copy, Clone)]
pub struct Rig {
    /// Horizontal trail behind the car on the flat.
    pub dist: i32,
    /// Height above the car.
    pub height: i32,
    /// Shortest horizontal arm the wall logic tries to keep before it orbits
    /// the eye sideways rather than shortening further.
    pub min_flat_dist: i32,
    /// Push out from a vertical surface (grows from `height` as the camera up
    /// leans over).
    pub wall_boom: i32,
    /// How much of the trail turns vertical while driving up a wall.
    pub wall_trail: i32,
    /// Ball cam's eye height in a split half.
    pub split_ball_height: i32,
    /// Added to the heading (the inspection view looks from three quarters).
    pub yaw_bias: i32,
}

impl Rig {
    pub const DEFAULT: Rig = Rig {
        dist: 800,
        height: 330,
        min_flat_dist: 650,
        wall_boom: 720,
        wall_trail: 500,
        split_ball_height: 200,
        yaw_bias: 0,
    };
    pub const WHEELS: Rig = Rig {
        dist: 430,
        height: 190,
        min_flat_dist: 400,
        wall_boom: 190,
        wall_trail: 0,
        split_ball_height: 200,
        yaw_bias: 512,
    };
}

/// What the renderer's view is, so the framing limits are in pixels and follow
/// the projection if it is ever widened.
#[derive(Copy, Clone)]
pub struct Screen {
    /// Projection plane distance in pixels.
    pub proj_h: i32,
    /// View width in pixels.
    pub w: i32,
    /// View height in pixels (half the screen in a split).
    pub h: i32,
}

impl Screen {
    pub const FULL: Screen = Screen { proj_h: 260, w: 320, h: 240 };
    pub const SPLIT: Screen = Screen { proj_h: 260, w: 320, h: 120 };
}

/// One call's choices.
#[derive(Copy, Clone)]
pub struct Request<'a> {
    pub rig: &'a Rig,
    pub screen: Screen,
    pub ball_cam: bool,
    /// Ordinary ball cam gives up some aim on the ball to keep your car in
    /// frame. A celebration wants the undiluted aim.
    pub hold_car: bool,
    pub split: bool,
}

/// Where the camera is and where it looks. The eye is world space, Y up;
/// `pitch` is positive looking down, as the renderer wants it.
#[derive(Copy, Clone, Debug, Default)]
pub struct Pose {
    pub eye: (i32, i32, i32),
    pub yaw: u16,
    pub pitch: i32,
    /// What the aim wanted before the view's own rate limit, for the diagnostic
    /// ring.
    pub desired_yaw: u16,
    pub desired_pitch: i32,
    pub heading: u16,
    pub ticks: i32,
}

/// Per-seat camera memory.
#[derive(Copy, Clone)]
pub struct CameraState {
    pub valid: bool,
    /// Sim tick this state was computed on.
    pub tick: u32,
    /// Smoothed camera up (Q12 unit), rebuilt each update from `tilt` and
    /// `tilt_az`.
    pub up: V3,
    /// Angle of the camera up from world up, `0..=2048` Q12 turns: 0 on the
    /// floor or in the air, 1024 on a wall, 2048 on the roof. Stored as an
    /// angle because a straight blend of two opposite vectors (roof to
    /// floor) has no direction part-way.
    pub tilt: i32,
    /// Yaw of the horizontal part of the camera up (the way a wall faces).
    pub tilt_az: u16,
    /// Direction from the car to the eye, as a yaw (Q12).
    pub az: u16,
    /// Horizontal arm length, uu.
    pub len: i32,
    /// Eye height above the car, uu (signed).
    pub rise: i32,
    /// Last side the eye was moved to round a wall, -1 or +1 (0: none yet).
    pub side: i8,
    /// Ticks the car has been off every surface, saturating.
    pub air_ticks: u8,
    /// Where the car was last frame, uu, to tell a kickoff from driving.
    pub car: (i32, i32, i32),
    /// Last good heading.
    pub heading: u16,
    pub yaw: u16,
    pub pitch: i32,
    /// The eye last returned, world space.
    pub eye: (i32, i32, i32),
}

impl CameraState {
    pub const EMPTY: Self = Self {
        valid: false,
        tick: 0,
        up: V3::new(0, 4096, 0),
        tilt: 0,
        tilt_az: 0,
        az: 0,
        len: 0,
        rise: 0,
        side: 0,
        air_ticks: 0,
        car: (0, 0, 0),
        heading: 0,
        yaw: 0,
        pitch: 0,
        eye: (0, 0, 0),
    };
}

// ---- tuning ---------------------------------------------------------------

/// Rate limits, per two ticks: the figures the old camera was tuned with
/// (96 Q12 per 30 Hz frame is about 252 degrees a second; 96 uu is 2900 uu/s).
const ANGLE_STEP: i32 = 96;
const LENGTH_STEP: i32 = 96;
const RISE_STEP: i32 = 96;
/// The arm shortens faster than it lengthens: a wall that closes in must not
/// be waited for.
const LENGTH_SHRINK_NUM: i32 = 3;
const LENGTH_SHRINK_DEN: i32 = 2;
/// The eye moves aside for a wall, and the air rise catches up, at this
/// multiple of the ordinary rate.
const FAST_NUM: i32 = 3;
const FAST_DEN: i32 = 2;
/// Camera-up slew per tick, Q12 turns: about six ticks to follow a surface
/// through a quarter turn, about seventeen to level out from the roof in the
/// air.
const TILT_STEP_SURFACE: i32 = 170;
const TILT_STEP_AIR: i32 = 120;
/// How fast the way a wall faces is followed, so a corner is a short swing.
const TILT_AZ_STEP: i32 = 256;
/// The least horizontal arm. Over the lip from a wall onto the roof the trail
/// behind the car and the push out from the wall point opposite ways and
/// cancel; this keeps the eye from collapsing under the car there.
const FLAT_MIN: i32 = 450;
/// Ticks off the surface before the camera treats the car as airborne.
const AIR_DELAY: i32 = 6;
/// Further than this in one frame (uu, per axis) and the car was moved, not
/// driven: 8 ticks at the 2300 uu/s top speed is 300.
const TELEPORT: i32 = 400;
/// The least horizontal distance the eye keeps from the car while it swings
/// round a wall, and how far into the hard margin it may go to keep it.
const FLAT_FLOOR: i32 = 300;
/// Ball cam, ball above the car: the arm grows by `rise / BALL_BACKOFF_DIV`
/// (at most `BALL_BACKOFF_MAX`) and the eye drops by `rise / BALL_DROP_DIV`
/// (not below `BALL_EYE_MIN` above the car).
const BALL_BACKOFF_DIV: i32 = 3;
const BALL_BACKOFF_MAX: i32 = 500;
const BALL_DROP_DIV: i32 = 4;
const BALL_EYE_MIN: i32 = 120;
const WALL_SLACK: i32 = 40;
/// Horizontal speed (sub-units per tick) above which an airborne car's travel
/// direction is the heading: 6 uu a tick, 360 uu/s.
const AIR_HEADING_SPEED: i32 = 6 << FP;
/// A horizontal nose shorter than this (Q12) is too close to vertical to give
/// a heading; the last good one is kept.
const NOSE_MIN: i32 = 256;
const NOSE_MIN_AIR: i32 = 1024;
/// Closer than this to the ball (horizontally) the car's own heading is used
/// in ball cam rather than a car to ball line that swings wildly.
const MIN_SEPARATION: i32 = 300;
/// Ball practically on the lens: aim down the nose instead.
const FALLBACK_AIM: i32 = 900;
/// How far past the nose the chase camera looks.
const CAR_AIM: i32 = 120;
/// Camera ticks after a kickoff during which the eye stays behind the car and
/// the arm shortens, rather than the eye moving round to a side.
const KICKOFF_BEHIND_TICKS: u8 = 240;
/// Margins the arm keeps from the arena walls. The preferred clearance is
/// what the arm tries to hold; the hard one is the least it may ever have.
const MARGIN_SOFT: i32 = 180;
const MARGIN_HARD: i32 = 50;
/// Height the eye keeps above the pitch and below the roof.
const SURFACE_CLEAR: i32 = 100;
/// Shortest 3D distance from the eye to the car.
const ARM_MIN: i32 = 300;
/// How much nearer the other edge of a blocked arc must be before the eye
/// gives up the side it was on (Q12 turns, about 45 degrees).
const SIDE_FLIP: i32 = 512;
const EDGE_PAD: i32 = 24;
const CAM_PITCH_MIN: i32 = -260;
const CAM_WALL_PITCH_MIN: i32 = -800;
const CAM_PITCH_MAX: i32 = 700;

// ---- the arena as half-spaces ---------------------------------------------

#[derive(Copy, Clone)]
struct Plane {
    nx: i32,
    nz: i32,
    soft: i32,
    hard: i32,
}

const N_PLANES: usize = 24;

/// The arena footprint `keep_inside` used to clamp a point to, as the 24
/// half-spaces `n . p <= offset` it is the intersection of: the four walls,
/// the four corner chamfers and the sixteen chords of the rounded joints.
const PLANES: [Plane; N_PLANES] = build_planes();

const fn build_planes() -> [Plane; N_PLANES] {
    let mut out = [Plane { nx: 0, nz: 0, soft: 0, hard: 0 }; N_PLANES];
    let mut n = 0;
    // Walls.
    let mut s = 0;
    while s < 2 {
        let sg = if s == 0 { 1 } else { -1 };
        out[n] = Plane {
            nx: 4096 * sg,
            nz: 0,
            soft: HALF_X - MARGIN_SOFT,
            hard: HALF_X - MARGIN_HARD,
        };
        n += 1;
        out[n] = Plane {
            nx: 0,
            nz: 4096 * sg,
            soft: HALF_Z - MARGIN_SOFT,
            hard: HALF_Z - MARGIN_HARD,
        };
        n += 1;
        s += 1;
    }
    // Chamfers and joint chords, one set per quadrant.
    let mut q = 0;
    while q < 4 {
        let sx = if q & 1 == 0 { 1 } else { -1 };
        let sz = if q & 2 == 0 { 1 } else { -1 };
        out[n] = Plane {
            nx: 2896 * sx,
            nz: 2896 * sz,
            soft: ((CORNER - MARGIN_SOFT * 3 / 2) * 2896) >> 12,
            hard: ((CORNER - MARGIN_HARD * 3 / 2) * 2896) >> 12,
        };
        n += 1;
        let mut j = 0;
        while j < 4 {
            let (nx, nz, off) = CORNER_JOINT_PLANES[j];
            out[n] = Plane {
                nx: nx * sx,
                nz: nz * sz,
                soft: (off >> 2) - MARGIN_SOFT,
                hard: (off >> 2) - MARGIN_HARD,
            };
            n += 1;
            j += 1;
        }
        q += 1;
    }
    out
}

/// How far a point is inside each plane, soft and hard margins.
struct Slack {
    soft: [i32; N_PLANES],
    hard: [i32; N_PLANES],
}

fn slack_at(x: i32, z: i32) -> Slack {
    let mut out = Slack { soft: [0; N_PLANES], hard: [0; N_PLANES] };
    let mut i = 0;
    while i < N_PLANES {
        let d = (PLANES[i].nx * x + PLANES[i].nz * z) >> 12;
        out.soft[i] = PLANES[i].soft - d;
        out.hard[i] = PLANES[i].hard - d;
        i += 1;
    }
    out
}

/// Distance along the direction `(s, c)` (Q12 sine and cosine of the yaw) to
/// the first plane, capped at 4096 uu. A point already outside a plane that
/// the ray leaves through reports zero.
fn ray(slack: &[i32; N_PLANES], bias: i32, s: i32, c: i32) -> i32 {
    let mut t = 4096;
    let mut i = 0;
    while i < N_PLANES {
        let den = (PLANES[i].nx * s + PLANES[i].nz * c) >> 12;
        // A ray nearly parallel to a plane meets it far beyond any arm.
        if den > 16 {
            let sl = (slack[i] + bias).max(0);
            // t * den: 4096 * 4096 fits an i32; the division only runs when
            // this plane is nearer than the best so far.
            if (sl << 12) < t * den {
                t = (sl << 12) / den;
            }
        }
        i += 1;
    }
    t
}

// ---- small helpers --------------------------------------------------------

#[inline]
fn wrap(a: i32) -> i32 {
    ((a + 2048) & 4095) - 2048
}

/// Move `cur` toward `target` by at most `step`, the short way round.
#[inline]
fn slew_angle(cur: u16, target: u16, step: i32) -> u16 {
    let d = wrap(target as i32 - cur as i32);
    ((cur as i32 + d.clamp(-step, step)) & 4095) as u16
}

#[inline]
fn fold(raw: u16) -> i32 {
    let raw = raw as i32;
    if raw > 2048 {
        raw - 4096
    } else {
        raw
    }
}

/// Q12 angle whose tangent is `px / proj_h`: a pixel offset in a view.
#[inline]
fn pixel_angle(px: i32, proj_h: i32) -> i32 {
    atan2_fine(px, proj_h) as i32
}

// ---- wall escape ----------------------------------------------------------

/// The arc of azimuths blocked by one plane for an arm of length `need`: the
/// directions whose ray meets the plane sooner than that. Centre and half
/// width, Q12 turns; `None` when the plane is far enough away.
fn blocked_arc(p: &Plane, slack: i32, need: i32) -> Option<(i32, i32)> {
    if slack >= need {
        return None;
    }
    // cos(half) = slack / need.
    let half = if slack <= -need {
        2048
    } else {
        let opp = isqrt_i32(need * need - slack * slack);
        atan2_fine(opp, slack) as i32
    };
    Some((atan2_fine(p.nx, p.nz) as i32, half))
}

/// The azimuth nearest `az` that an arm of length `need` can take. `az`
/// itself when it is free; otherwise the nearer edge of the merged blocked arc
/// around it, except that the side the eye last used is kept unless the other
/// edge is much nearer, so the camera never flips sides on a whim.
fn escape(slack: &[i32; N_PLANES], az: u16, need: i32, side: &mut i8) -> u16 {
    // Arcs relative to `az`, as [lo, hi] on the unrolled line. An arc can wrap
    // the circle, so each is tried at -1, 0 and +1 turns.
    let mut arcs = [(0i32, 0i32); 12];
    let mut n = 0;
    let mut i = 0;
    while i < N_PLANES && n < arcs.len() {
        if let Some((centre, half)) = blocked_arc(&PLANES[i], slack[i], need) {
            let rel = wrap(centre - az as i32);
            arcs[n] = (rel - half, rel + half);
            n += 1;
        }
        i += 1;
    }
    let (mut lo, mut hi) = (0, 0);
    let mut inside = false;
    for &(a, b) in &arcs[..n] {
        for k in -1..=1 {
            let (a, b) = (a + 4096 * k, b + 4096 * k);
            if a <= 0 && b >= 0 {
                inside = true;
                lo = lo.min(a);
                hi = hi.max(b);
            }
        }
    }
    if !inside {
        return az;
    }
    // Grow to the arcs it touches until it stops growing.
    for _ in 0..n {
        let mut grew = false;
        for &(a, b) in &arcs[..n] {
            for k in -1..=1 {
                let (a, b) = (a + 4096 * k, b + 4096 * k);
                if a <= hi && b >= lo && (a < lo || b > hi) {
                    lo = lo.min(a);
                    hi = hi.max(b);
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    if hi - lo >= 4096 {
        // Nowhere is clear: leave it to the ray clamp.
        return az;
    }
    let down = -lo + EDGE_PAD;
    let up = hi + EDGE_PAD;
    let pick_up = match *side {
        s if s > 0 => up <= down + SIDE_FLIP,
        s if s < 0 => !(down <= up + SIDE_FLIP),
        _ => up <= down,
    };
    *side = if pick_up { 1 } else { -1 };
    let turn = if pick_up { up } else { -down };
    ((az as i32 + turn) & 4095) as u16
}

// ---- the update -----------------------------------------------------------

/// Advance one seat's camera to `now` (a sim tick) and return its pose.
pub fn update(
    st: &mut CameraState,
    s: &Sim,
    subject: &Car,
    rq: &Request,
    now: u32,
) -> Pose {
    let rig = rq.rig;
    let car_at = (subject.p.x >> FP, subject.p.y >> FP, subject.p.z >> FP);
    // A car that has moved further than it can drive in the longest gap
    // between frames was put somewhere (a kickoff): start over from it.
    let teleported = {
        let d = (car_at.0 - st.car.0, car_at.1 - st.car.1, car_at.2 - st.car.2);
        d.0.abs().max(d.1.abs()).max(d.2.abs()) > TELEPORT
    };
    let valid = st.valid && !teleported;
    if !valid {
        st.side = 0;
        st.air_ticks = 0;
    }
    let ticks = now.wrapping_sub(st.tick).clamp(1, 8) as i32;
    let ang_step = ANGLE_STEP * ticks / 2;
    let len_step = LENGTH_STEP * ticks / 2;
    let rise_step = RISE_STEP * ticks / 2;

    let (_, car_up, car_fwd) = subject.basis();
    let (px, py, pz) = car_at;
    let (bx, by, bz) = (s.ball.p.x >> FP, s.ball.p.y >> FP, s.ball.p.z >> FP);

    // A car skipping over a ramp or a bump is off the surface for a few ticks
    // at a time; only a longer spell counts as flying, so the camera does not
    // flip between the two modes with every hop.
    st.air_ticks = if subject.grounded {
        0
    } else {
        (st.air_ticks as i32 + ticks).min(255) as u8
    };
    let on_surface = subject.grounded || (st.air_ticks as i32) < AIR_DELAY;

    // Camera up: the surface while on one, world up in the air.
    let (target_tilt, target_az) = if on_surface {
        let h = isqrt_i32(car_up.x * car_up.x + car_up.z * car_up.z);
        let az = if h >= 64 { Some(atan2_fine(car_up.x, car_up.z)) } else { None };
        (atan2_fine(h, car_up.y) as i32, az)
    } else {
        (0, None)
    };
    if valid {
        let step = if on_surface { TILT_STEP_SURFACE } else { TILT_STEP_AIR } * ticks;
        st.tilt += (target_tilt - st.tilt).clamp(-step, step);
        if let Some(az) = target_az {
            // On the flat there is no wall to face: take the new one at once.
            st.tilt_az = if st.tilt < 64 { az } else { slew_angle(st.tilt_az, az, TILT_AZ_STEP * ticks) };
        }
    } else {
        st.tilt = target_tilt;
        st.tilt_az = target_az.unwrap_or(0);
    }
    let (ts, tc) = (sin_q12(st.tilt as u16), cos_q12(st.tilt as u16));
    let up = V3::new(
        (ts * sin_q12(st.tilt_az)) >> 12,
        tc,
        (ts * cos_q12(st.tilt_az)) >> 12,
    );
    st.up = up;
    let lean = 4096 - up.y.clamp(0, 4096);

    // Heading.
    let nose = isqrt_i32(car_fwd.x * car_fwd.x + car_fwd.z * car_fwd.z).min(4096);
    let own_heading = if on_surface {
        if nose >= NOSE_MIN || !valid {
            atan2_fine(car_fwd.x, car_fwd.z)
        } else {
            st.heading
        }
    } else {
        let flat_v = V3::new(subject.v.x, 0, subject.v.z).len_xz();
        if flat_v >= AIR_HEADING_SPEED {
            atan2_fine(subject.v.x, subject.v.z)
        } else if nose >= NOSE_MIN_AIR {
            atan2_fine(car_fwd.x, car_fwd.z)
        } else if valid {
            st.heading
        } else {
            atan2_fine(car_fwd.x, car_fwd.z)
        }
    };
    let (dx, dz) = (bx - px, bz - pz);
    let line_to_ball = rq.ball_cam && dx * dx + dz * dz > MIN_SEPARATION * MIN_SEPARATION;
    st.heading = own_heading;
    let heading = if line_to_ball {
        atan2_fine(dx, dz)
    } else {
        own_heading
    };
    let heading = ((heading as i32 + rig.yaw_bias) & 4095) as u16;
    let (hs, hc) = (sin_q12(heading), cos_q12(heading));

    // How much of the trail stays horizontal: the nose shortens toward zero up
    // a wall, but only while a surface is under the car.
    let flat_factor = if rq.ball_cam || !on_surface {
        4096
    } else {
        4096 - (((4096 - nose) * lean) >> 12)
    };
    let eye_height = if rq.ball_cam && rq.hold_car && rq.split {
        rig.split_ball_height
    } else {
        rig.height
    };
    // A ball high over the car does not fit under the lens from the usual
    // eye: back off and drop the eye with the ball's height, so the car stays
    // at the foot of the frame and the ball under its top.
    let (back_off, drop) = if rq.ball_cam && rq.hold_car {
        let rise = (by - py).max(0);
        (
            (rise / BALL_BACKOFF_DIV).min(BALL_BACKOFF_MAX),
            (rise / BALL_DROP_DIV).min(eye_height - BALL_EYE_MIN).max(0),
        )
    } else {
        (0, 0)
    };
    let trail = ((rig.dist * flat_factor) >> 12) + back_off;
    let boom = rig.height + (((rig.wall_boom - rig.height) * lean) >> 12);
    let ax = -((hs * trail) >> 12) + ((up.x * boom) >> 12);
    let az_vec = -((hc * trail) >> 12) + ((up.z * boom) >> 12);
    let mut want_len = isqrt_i32(ax * ax + az_vec * az_vec);
    let mut want_az = if want_len >= 48 {
        atan2_fine(ax, az_vec)
    } else if valid {
        st.az
    } else {
        atan2_fine(-hs, -hc)
    };
    if want_len < FLAT_MIN {
        // Prefer pushing out from the surface, then the way it was going.
        let push = isqrt_i32((up.x * boom >> 12).pow(2) + (up.z * boom >> 12).pow(2));
        if push >= 100 {
            want_az = atan2_fine(up.x, up.z);
        }
        want_len = FLAT_MIN;
    }

    let wall_trail = (((car_fwd.y * rig.wall_trail) >> 12) * lean) >> 12;
    let want_rise = ((up.y * (eye_height - drop)) >> 12) - wall_trail;
    let want_rise = want_rise.clamp(SURFACE_CLEAR - py, CEIL - SURFACE_CLEAR - py);

    // Walls: move the azimuth off a blocked arc, never inside the kickoff
    // window, where the approved view shortens the arm behind the car.
    // The arm pivots on the car, except where the car is outside the arena
    // proper (in the net): there it pivots on the nearest point inside, so
    // the eye stays on the pitch side of the goal line and looks in.
    let (qx, qz, slack) = pivot(px, pz);
    // Inside the kickoff window the approved view keeps the eye behind the
    // car with a short arm, so it only moves aside for less than the minimum.
    let need = if s.kickoff_ticks() >= KICKOFF_BEHIND_TICKS {
        want_len.min(rig.min_flat_dist)
    } else {
        ARM_MIN
    };
    let target_az = escape(&slack.soft, want_az, need, &mut st.side);

    // Arm state, each part limited on its own.
    // Moving aside for a wall takes the faster of two rates: the eye must be
    // off the blocked arc before the car reaches the wall, not after.
    let swing = if target_az != want_az { ang_step * FAST_NUM / FAST_DEN } else { ang_step };
    if valid {
        st.az = slew_angle(st.az, target_az, swing);
    } else {
        st.az = target_az;
    }
    let (es, ec) = (sin_q12(st.az), cos_q12(st.az));
    // The arm is sized for the direction it is turning to, so it does not
    // wither while it swings round.
    let room = ray(&slack.soft, 0, sin_q12(target_az), cos_q12(target_az));
    let goal_len = want_len.min(room);
    if valid {
        let d = goal_len - st.len;
        st.len += d.clamp(-len_step * LENGTH_SHRINK_NUM / LENGTH_SHRINK_DEN, len_step);
        // Falling away from a wall or roof the eye must catch up quickly or it
        // is left looking up at the car from far below.
        let rs = if on_surface { rise_step } else { rise_step * FAST_NUM / FAST_DEN };
        st.rise += (want_rise - st.rise).clamp(-rs, rs);
    } else {
        st.len = goal_len;
        st.rise = want_rise;
    }

    // Final eye: the ray clamp at the hard margin, the pitch and roof, and a
    // minimum arm.
    let eye_y = (py + st.rise).clamp(SURFACE_CLEAR, CEIL - SURFACE_CLEAR);
    let dy = eye_y - py;
    let floor_len = isqrt_i32((ARM_MIN * ARM_MIN - dy * dy).max(0));
    // While the eye is still swinging it may sit in the wall margin, but never
    // closer to the car than the floor.
    let len = st
        .len
        .min(ray(&slack.hard, 0, es, ec))
        .max(floor_len.max(FLAT_FLOOR).min(ray(&slack.hard, WALL_SLACK, es, ec)));
    let eye = (qx + ((es * len) >> 12), eye_y, qz + ((ec * len) >> 12));

    // ---- look ----
    let mut yaw_want;
    let pitch_want;
    let flat_to_ball = isqrt_i32((bx - eye.0) * (bx - eye.0) + (bz - eye.2) * (bz - eye.2));
    if rq.ball_cam && flat_to_ball >= FALLBACK_AIM / 2 {
        let car_flat = isqrt_i32((px - eye.0) * (px - eye.0) + (pz - eye.2) * (pz - eye.2)).max(1);
        let ball_flat = flat_to_ball.max(1);
        yaw_want = atan2_fine(bx - eye.0, bz - eye.2) as i32;
        if rq.hold_car {
            let sc = rq.screen;
            let half_h = sc.h / 2;
            let half_w = sc.w / 2;
            // Pitches to each, positive down.
            let p_ball = fold(atan2_fine(eye.1 - by, ball_flat));
            let p_car = fold(atan2_fine(eye.1 - py, car_flat));
            let car_low = pixel_angle(half_h / 3, sc.proj_h);
            let car_max = pixel_angle(half_h * 7 / 10, sc.proj_h);
            let top = pixel_angle(half_h * 4 / 5, sc.proj_h);
            // Car on the lower third; look higher if that loses the ball off
            // the top; the car's own limit has the last word.
            let mut p = p_car - car_low;
            p = p.min(p_ball + top);
            p = p.max(p_car - car_max);
            pitch_want = p;
            // Same on the horizontal: the ball centred unless that pushes
            // the car toward the edge.
            let y_car = atan2_fine(px - eye.0, pz - eye.2) as i32;
            let car_w = pixel_angle(half_w * 7 / 10, sc.proj_h);
            let d = wrap(y_car - yaw_want);
            let over = (d.abs() - car_w).max(0);
            yaw_want = (yaw_want + d.signum() * over) & 4095;
        } else {
            // Half the ball's rise, as the celebration always framed it.
            let ay = py + (by - py) / 2;
            pitch_want = fold(atan2_q12(eye.1 - ay, ball_flat));
        }
    } else {
        let (ox, oz) = if rq.ball_cam {
            ((hs * FALLBACK_AIM) >> 12, (hc * FALLBACK_AIM) >> 12)
        } else {
            // The nose's horizontal part, so a car pitched up a wall looks
            // along the wall rather than at a point inside it.
            let reach = (flat_factor * CAR_AIM) >> 12;
            ((hs * reach) >> 12, (hc * reach) >> 12)
        };
        let aim_y = if rq.ball_cam {
            py
        } else {
            py + ((up.y * CAR_HALF_H) >> 12) + ((((car_fwd.y * CAR_AIM) >> 12) * lean) >> 12)
        };
        let (aim_x, aim_z) = (px + ox, pz + oz);
        yaw_want = atan2_fine(aim_x - eye.0, aim_z - eye.2) as i32;
        let flat = isqrt_i32((aim_x - eye.0) * (aim_x - eye.0) + (aim_z - eye.2) * (aim_z - eye.2)).max(1);
        // The SDK's octant-linear atan2, not the fine one: the chase camera's
        // approved framing (the car a little over half way down at kickoff)
        // was tuned with it, and the exact angle would nudge every shot up.
        pitch_want = fold(atan2_q12(eye.1 - aim_y, flat));
    }
    let pitch_min = CAM_PITCH_MIN + (((CAM_WALL_PITCH_MIN - CAM_PITCH_MIN) * lean) >> 12);
    let desired_pitch = pitch_want.clamp(pitch_min, CAM_PITCH_MAX);
    let desired_yaw = (yaw_want & 4095) as u16;

    let (mut yaw, mut pitch) = if valid {
        (
            slew_angle(st.yaw, desired_yaw, swing),
            st.pitch + (desired_pitch - st.pitch).clamp(-swing, swing),
        )
    } else {
        (desired_yaw, desired_pitch)
    };
    // The one promise the view keeps whatever the rate limits are doing: the
    // car is inside the frame, with a margin. The aim above is a wish; this is
    // applied after it has been slewed, so a fast eye cannot outrun the lens.
    if rq.hold_car {
        let sc = rq.screen;
        let car_flat = isqrt_i32((px - eye.0) * (px - eye.0) + (pz - eye.2) * (pz - eye.2)).max(1);
        let to_car_yaw = atan2_fine(px - eye.0, pz - eye.2) as i32;
        let limit_x = pixel_angle(sc.w / 2 * 4 / 5, sc.proj_h);
        let d = wrap(to_car_yaw - yaw as i32);
        if d.abs() > limit_x {
            yaw = ((yaw as i32 + d - d.signum() * limit_x) & 4095) as u16;
        }
        let to_car_pitch = fold(atan2_fine(eye.1 - py, car_flat));
        let limit_y = pixel_angle(sc.h / 2 * 17 / 20, sc.proj_h);
        let rel = to_car_pitch - pitch;
        if rel.abs() > limit_y {
            pitch = to_car_pitch - rel.signum() * limit_y;
        }
    }

    st.valid = true;
    st.tick = now;
    st.yaw = yaw;
    st.pitch = pitch;
    st.eye = eye;
    st.car = car_at;
    Pose { eye, yaw, pitch, desired_yaw, desired_pitch, heading, ticks }
}

/// The point the arm pivots on and its slack: the car's own position when it is
/// inside the arena with the hard margin, otherwise the nearest point that is.
fn pivot(x: i32, z: i32) -> (i32, i32, Slack) {
    let mut slack = slack_at(x, z);
    if slack.hard.iter().all(|&s| s >= 0) {
        return (x, z, slack);
    }
    let (mut qx, mut qz) = (x, z);
    for _ in 0..3 {
        for i in 0..N_PLANES {
            let sl = slack.hard[i];
            if sl < 0 {
                qx += (PLANES[i].nx * sl) >> 12;
                qz += (PLANES[i].nz * sl) >> 12;
                slack = slack_at(qx, qz);
            }
        }
    }
    (qx, qz, slack)
}

/// Whether a world point (uu) is inside the arena with the least margin the
/// camera ever accepts: the hard margin less the little it may borrow while it
/// swings round a wall. For tests and callers that must not place something
/// outside.
pub fn inside_arena(x: i32, z: i32) -> bool {
    slack_at(x, z).hard.iter().all(|&s| s + WALL_SLACK >= 0)
}
