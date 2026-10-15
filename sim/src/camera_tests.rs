// SPDX-License-Identifier: GPL-2.0-or-later
//! Host scenarios for the chase camera. Each one drives the real simulation,
//! runs the real camera and projects the car and ball through the same view
//! the renderer builds, so "the car is on screen" is measured, not assumed.
//!
//! `cargo test camera_tests::print -- --ignored --nocapture` prints every
//! scenario's numbers; set `PRINT=<n>` to also list every n-th frame.

use crate::camera::{inside_arena, update, CameraState, Pose, Request, Rig, Screen};
use crate::{uu, Input, Sim, FP};
use psx_math::int32::isqrt_i32;
use psx_math::sincos::{cos_q12, sin_q12};

/// The most the eye may move between two frames, per tick elapsed between
/// them. The arm's own rate limits allow about 140 together (72 uu a tick
/// shrinking, 72 rising, a fast orbit at 900 uu) and the worst frame measured
/// over four whole demo matches is 151. For scale, the old camera's worst frame
/// in the scripted runs was a single 562 uu move.
const MAX_EYE_JUMP_PER_TICK: i32 = 160;

/// Project a world point (uu, Y up) through a pose, the way `look_from` builds
/// the view: Y flips to render space, then the three basis rows.
fn project(pose: &Pose, screen: Screen, p: (i32, i32, i32)) -> Option<(i32, i32)> {
    let pitch = pose.pitch.rem_euclid(4096) as u16;
    let (sp, cp) = (sin_q12(pitch), cos_q12(pitch));
    let (sy, cy) = (sin_q12(pose.yaw), cos_q12(pose.yaw));
    let rt = [cy, 0, -sy];
    let up = [(-sy * sp) >> 12, cp, (-cy * sp) >> 12];
    let f = [(sy * cp) >> 12, sp, (cy * cp) >> 12];
    let d = [p.0 - pose.eye.0, -(p.1 - pose.eye.1), p.2 - pose.eye.2];
    let dot = |r: [i32; 3]| (r[0] * d[0] + r[1] * d[1] + r[2] * d[2]) >> 12;
    let (x, y, z) = (dot(rt), dot(up), dot(f));
    if z <= 20 {
        return None;
    }
    Some((screen.w / 2 + x * screen.proj_h / z, screen.h / 2 + y * screen.proj_h / z))
}

fn on_screen(p: Option<(i32, i32)>, screen: Screen) -> bool {
    matches!(p, Some((x, y)) if x >= 0 && x < screen.w && y >= 0 && y < screen.h)
}

#[derive(Debug, Default, Clone)]
struct Stats {
    frames: u32,
    car_off: u32,
    ball_off: u32,
    /// Largest eye move between frames, per elapsed tick, and when.
    max_jump: i32,
    jump_at: u32,
    /// Shortest and longest 3D distance from the eye to the car.
    min_arm: i32,
    max_arm: i32,
    /// Frames the eye was outside the arena (hard margin).
    outside: u32,
    /// Range of the car's and ball's screen y.
    car_y: (i32, i32),
    ball_y: (i32, i32),
    /// Turns of the view yaw from one direction to the other, ignoring
    /// movements of a unit or two.
    yaw_reversals: u32,
}

#[derive(Clone)]
struct Scenario {
    name: &'static str,
    ball_cam: bool,
    split: bool,
    ticks: u32,
    /// Render every N ticks (1: 60 fps, 2: 30 fps).
    every: u32,
    /// Past the kickoff window, where the camera may move round to a side.
    settled: bool,
    /// Whose camera: 0 the player, 1 the opponent.
    seat: usize,
    proj_h: i32,
    /// Flip ball cam every this many ticks (0: never).
    toggle_every: u32,
    setup: fn(&mut Sim),
    input: fn(u32, &Sim) -> Input,
}

impl Scenario {
    fn new(name: &'static str, ticks: u32, setup: fn(&mut Sim), input: fn(u32, &Sim) -> Input) -> Self {
        Scenario {
            name,
            ball_cam: false,
            split: false,
            ticks,
            every: 1,
            settled: true,
            seat: 0,
            proj_h: 260,
            toggle_every: 0,
            setup,
            input,
        }
    }
    fn ball(mut self) -> Self {
        self.ball_cam = true;
        self
    }
    fn every(mut self, n: u32) -> Self {
        self.every = n;
        self
    }
    fn split(mut self) -> Self {
        self.split = true;
        self
    }
    fn fresh(mut self) -> Self {
        self.settled = false;
        self
    }
    fn toggling(mut self, n: u32) -> Self {
        self.toggle_every = n;
        self
    }
    fn proj(mut self, h: i32) -> Self {
        self.proj_h = h;
        self
    }
}

fn run(sc: &Scenario) -> Stats {
    let mut st = CameraState::EMPTY;
    let mut s = Sim::new();
    s.opponent_ai = false;
    (sc.setup)(&mut s);
    if sc.settled {
        s.kickoff_ticks = 255;
    }
    let mut screen = if sc.split { Screen::SPLIT } else { Screen::FULL };
    screen.proj_h = sc.proj_h;
    let rig = Rig::DEFAULT;
    let mut rq = Request { rig: &rig, screen, ball_cam: sc.ball_cam, hold_car: true, split: sc.split };
    let print: u32 = std::env::var("PRINT").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    if print > 0 {
        println!(
            "\n=== {} (ball_cam={} split={} every {} proj {}) ===",
            sc.name, sc.ball_cam, sc.split, sc.every, sc.proj_h
        );
    }
    let mut out = Stats {
        min_arm: i32::MAX,
        car_y: (i32::MAX, i32::MIN),
        ball_y: (i32::MAX, i32::MIN),
        ..Stats::default()
    };
    let (mut last_eye, mut last_t): (Option<(i32, i32, i32)>, u32) = (None, 0);
    let (mut last_yaw, mut last_dyaw) = (None::<u16>, 0i32);
    for t in 0..sc.ticks {
        let inp = (sc.input)(t, &s);
        s.tick(&inp);
        if t % sc.every != 0 {
            continue;
        }
        if sc.toggle_every > 0 && t > 0 && t % sc.toggle_every < sc.every {
            rq.ball_cam = !rq.ball_cam;
        }
        let subject = if sc.seat == 0 { s.car } else { s.opponent };
        let pose = update(&mut st, &s, &subject, &rq, t);
        out.frames += 1;
        let car = (subject.p.x >> FP, subject.p.y >> FP, subject.p.z >> FP);
        let ball = (s.ball.p.x >> FP, s.ball.p.y >> FP, s.ball.p.z >> FP);
        let (cs, bs) = (project(&pose, screen, car), project(&pose, screen, ball));
        out.car_off += !on_screen(cs, screen) as u32;
        out.ball_off += !on_screen(bs, screen) as u32;
        if let Some((_, y)) = cs {
            out.car_y = (out.car_y.0.min(y), out.car_y.1.max(y));
        }
        if let Some((_, y)) = bs {
            out.ball_y = (out.ball_y.0.min(y), out.ball_y.1.max(y));
        }
        let d = (pose.eye.0 - car.0, pose.eye.1 - car.1, pose.eye.2 - car.2);
        let arm = isqrt_i32(d.0 * d.0 + d.1 * d.1 + d.2 * d.2);
        out.min_arm = out.min_arm.min(arm);
        out.max_arm = out.max_arm.max(arm);
        out.outside += !inside_arena(pose.eye.0, pose.eye.2) as u32;
        if let Some(e) = last_eye {
            let d = (pose.eye.0 - e.0, pose.eye.1 - e.1, pose.eye.2 - e.2);
            let j = isqrt_i32(d.0 * d.0 + d.1 * d.1 + d.2 * d.2) / (t - last_t).max(1) as i32;
            if j > out.max_jump {
                out.max_jump = j;
                out.jump_at = t;
            }
        }
        if let Some(ly) = last_yaw {
            let dy = ((pose.yaw as i32 - ly as i32 + 2048) & 4095) - 2048;
            if dy.abs() >= 3 {
                if last_dyaw != 0 && dy.signum() != last_dyaw.signum() {
                    out.yaw_reversals += 1;
                }
                last_dyaw = dy;
            }
        }
        last_yaw = Some(pose.yaw);
        last_eye = Some(pose.eye);
        last_t = t;
        if print > 0 && t % print == 0 {
            println!(
                "{t:>4} car ({:>5},{:>5},{:>5}) up.y {:>5} g {} | eye ({:>5},{:>5},{:>5}) arm {:>4} L {:>4} R {:>5} tilt {:>4} az {:>4} | yaw {:>4} pit {:>4} | car {:?} ball {:?}{}",
                car.0, car.1, car.2, subject.up.y, subject.grounded as i32, pose.eye.0, pose.eye.1, pose.eye.2,
                arm, st.len, st.rise, st.tilt, st.az, pose.yaw, pose.pitch, cs, bs,
                if on_screen(cs, screen) { "" } else { "  <-- CAR OFF" }
            );
        }
    }
    if print > 0 {
        println!("{out:?}");
    }
    out
}

// ---- inputs and set-ups ---------------------------------------------------

fn throttle() -> Input {
    Input { throttle: 128, ..Input::default() }
}
fn boost() -> Input {
    Input { throttle: 128, boost: true, ..Input::default() }
}
fn always_boost(_: u32, _: &Sim) -> Input {
    boost()
}
fn always_throttle(_: u32, _: &Sim) -> Input {
    throttle()
}

fn kickoff_only(s: &mut Sim) {
    s.kickoff();
}

fn arc_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = 0;
    s.car.p.z = uu(-300);
    s.car.yaw = 0;
    s.ball.p.x = uu(2500);
    s.ball.p.z = uu(2500);
}

fn arc_input(_: u32, _: &Sim) -> Input {
    Input { throttle: 128, steer: 50, ..Input::default() }
}

fn turn_setup(s: &mut Sim) {
    s.kickoff();
    s.car.v.z = 0;
}

/// Full lock to the right after a second of straight driving: out of the
/// back-wall spot, round, up the end wall, over the roof and back down.
fn turn_input(t: u32, _: &Sim) -> Input {
    if t < 60 {
        throttle()
    } else {
        Input { throttle: 128, steer: 128, boost: true, ..Input::default() }
    }
}

fn backing_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = uu(1500);
    s.car.p.z = uu(-3600);
    s.car.yaw = 2048;
    // 900 uu/s is 15 uu a tick; velocities are sub-units per tick.
    s.car.v.z = -uu(15);
    s.ball.p.x = uu(-500);
    s.ball.p.z = uu(1500);
}

fn backing_input(t: u32, _: &Sim) -> Input {
    if t < 30 {
        throttle()
    } else if t < 100 {
        Input { throttle: 128, steer: 128, handbrake: true, ..Input::default() }
    } else {
        throttle()
    }
}

fn side_wall_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = uu(2500);
    s.car.p.z = 0;
    s.car.yaw = 1024 - 300;
    s.ball.p.x = uu(-1500);
    s.ball.p.z = uu(1500);
}

fn end_wall_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = uu(-1500);
    s.car.p.z = uu(1500);
    s.car.yaw = 0;
    s.ball.p.x = uu(1500);
    s.ball.p.z = uu(-1000);
}

fn air_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = 0;
    s.car.p.z = uu(-1000);
    s.car.yaw = 0;
    s.ball.p.z = uu(2000);
}

/// Jump, pitch back for a second, then air roll as well: the car ends up
/// tumbling for most of a second.
fn air_input(t: u32, _: &Sim) -> Input {
    match t {
        0..=5 => boost(),
        6..=20 => Input {
            throttle: 128,
            jump_pressed: t == 6,
            jump_held: true,
            boost: true,
            ..Input::default()
        },
        21..=120 => Input {
            throttle: 128,
            boost: true,
            pitch: 128,
            air_roll: t > 60,
            steer: if t > 60 { 128 } else { 0 },
            ..Input::default()
        },
        _ => Input::default(),
    }
}

fn flip_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = 0;
    s.car.p.z = uu(-1500);
    s.car.yaw = 0;
    s.ball.p.z = uu(2000);
}

fn flip_input(t: u32, _: &Sim) -> Input {
    match t {
        0..=29 => boost(),
        30 => Input { throttle: 128, jump_pressed: true, jump_held: true, ..Input::default() },
        31..=40 => Input { throttle: 128, jump_held: true, ..Input::default() },
        41 => Input { throttle: 128, jump_pressed: true, pitch: -128, ..Input::default() },
        _ => throttle(),
    }
}

fn behind_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = 0;
    s.car.p.z = 0;
    s.car.yaw = 0;
    s.ball.p.z = uu(-2500);
    s.ball.p.x = uu(600);
}

fn past_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = uu(-800);
    s.car.p.z = uu(-1500);
    s.car.yaw = 0;
    s.ball.p.x = 0;
    s.ball.p.z = 0;
}

fn high_ball_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = 0;
    s.car.p.z = 0;
    s.car.yaw = 0;
    s.ball.p.z = uu(1200);
    s.ball.p.y = uu(1500);
}

/// A lofted ball and a car driving under it: the ball is in the air and
/// moving the whole time.
fn lob_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = 0;
    s.car.p.z = uu(-2000);
    s.car.yaw = 0;
    s.ball.p.z = uu(-200);
    s.ball.p.y = uu(500);
    s.ball.v.y = uu(24);
    s.ball.v.z = uu(2);
    s.ball.grounded = false;
}

fn net_setup(s: &mut Sim) {
    s.kickoff();
    s.car.p.x = 0;
    s.car.p.z = uu(3800);
    s.car.yaw = 0;
    s.ball.p.z = 0;
}

fn spot_setup(spot: usize) -> fn(&mut Sim) {
    fn at<const N: usize>(s: &mut Sim) {
        s.kickoff_spot = N;
        s.kickoff();
    }
    match spot {
        0 => at::<0>,
        1 => at::<1>,
        2 => at::<2>,
        3 => at::<3>,
        _ => at::<4>,
    }
}

// ---- the scenario set ------------------------------------------------------

fn kickoff(ball: bool, every: u32) -> Scenario {
    let s = Scenario::new("kickoff straight boost", 240, kickoff_only, always_boost)
        .every(every)
        .fresh();
    if ball {
        s.ball()
    } else {
        s
    }
}

/// Every scenario the camera is held to.
fn all() -> Vec<Scenario> {
    let mut v = Vec::new();
    for ball in [false, true] {
        let c = |s: Scenario| if ball { s.ball() } else { s };
        for every in [1, 2] {
            v.push(kickoff(ball, every));
            v.push(
                c(Scenario::new("full steer turn after kickoff", 420, turn_setup, turn_input))
                    .every(every)
                    .fresh(),
            );
            v.push(c(Scenario::new("steady gentle arc on open floor", 360, arc_setup, arc_input)).every(every));
        }
        v.push(c(Scenario::new("defender turns round near the back wall", 240, backing_setup, backing_input)));
        v.push(c(Scenario::new("side wall, up it and over the roof", 420, side_wall_setup, always_boost)));
        v.push(c(Scenario::new("end wall and up it", 420, end_wall_setup, always_boost)));
        v.push(c(Scenario::new("jump, pitch back, air roll", 300, air_setup, air_input)));
        v.push(c(Scenario::new("front flip off the floor", 200, flip_setup, flip_input)));
        v.push(c(Scenario::new("drive past the ball", 240, past_setup, always_boost)));
        v.push(c(Scenario::new("car drives into the goal", 240, net_setup, always_boost)));
        v.push(c(Scenario::new("lofted ball, car drives under it", 160, lob_setup, always_boost)));
        // The same climbs and tumbles in a split half, where the view is 120 high.
        v.push(
            c(Scenario::new("side wall, up it and over the roof", 420, side_wall_setup, always_boost))
                .split()
                .every(2),
        );
        v.push(c(Scenario::new("jump, pitch back, air roll", 300, air_setup, air_input)).split().every(2));
        for spot in 0..5 {
            for seat in 0..2 {
                let mut s = c(Scenario::new("kickoff spot", 240, spot_setup(spot), always_boost))
                    .every(2)
                    .fresh();
                s.seat = seat;
                v.push(s.clone());
                v.push(s.split());
            }
        }
    }
    // Ball cam switched on and off under the player's thumb, every second and a half.
    v.push(Scenario::new("ball cam toggled while turning", 420, turn_setup, turn_input).toggling(90).fresh());
    v.push(Scenario::new("ball cam toggled over the walls", 420, side_wall_setup, always_boost).toggling(75));
    v.push(Scenario::new("ball cam toggled in a split half", 300, air_setup, air_input).toggling(60).split().every(2));
    v.push(Scenario::new("ball behind a driving car", 240, behind_setup, always_boost).ball());
    v.push(Scenario::new("ball cam, ball high over the car", 120, high_ball_setup, always_throttle).ball());
    v
}

fn describe(sc: &Scenario, s: &Stats) -> String {
    format!(
        "{:<42} bc={} split={} every={} proj={} | car-off {:>3} ball-off {:>3} of {:>3} | jump {:>3}/tick@{:>3} arm {:>4}..{:>4} outside {:>3} rev {:>2} | car y {:?} ball y {:?}",
        sc.name, sc.ball_cam as i32, sc.split as i32, sc.every, sc.proj_h, s.car_off, s.ball_off, s.frames,
        s.max_jump, s.jump_at, s.min_arm, s.max_arm, s.outside, s.yaw_reversals, s.car_y, s.ball_y
    )
}

#[test]
#[ignore = "diagnostic: prints every scenario"]
fn print_all_scenarios() {
    for sc in all() {
        println!("{}", describe(&sc, &run(&sc)));
    }
}

// ---- the promises -----------------------------------------------------------

#[test]
fn the_car_is_on_screen_in_every_frame_of_every_scenario() {
    let mut bad = Vec::new();
    for sc in all() {
        let s = run(&sc);
        if s.car_off > 0 {
            bad.push(describe(&sc, &s));
        }
    }
    assert!(bad.is_empty(), "car off screen:\n{}", bad.join("\n"));
}

#[test]
fn the_car_stays_on_screen_with_a_wider_field_too() {
    // The projection is Manny's call, but the camera has to hold whichever he
    // picks: the framing limits are in pixels of the view, not degrees.
    for proj in [200, 160] {
        let mut bad = Vec::new();
        for sc in all() {
            let sc = sc.proj(proj);
            let s = run(&sc);
            if s.car_off > 0 {
                bad.push(describe(&sc, &s));
            }
        }
        assert!(bad.is_empty(), "proj {proj}: car off screen:\n{}", bad.join("\n"));
    }
}

#[test]
fn the_eye_never_jumps_and_never_leaves_the_arena() {
    let mut bad = Vec::new();
    for sc in all() {
        let s = run(&sc);
        if s.max_jump > MAX_EYE_JUMP_PER_TICK || s.outside > 0 {
            bad.push(describe(&sc, &s));
        }
    }
    assert!(bad.is_empty(), "jump or outside:\n{}", bad.join("\n"));
}

#[test]
fn the_eye_keeps_clear_of_the_car() {
    let mut bad = Vec::new();
    for sc in all() {
        let s = run(&sc);
        if s.min_arm < 300 {
            bad.push(describe(&sc, &s));
        }
    }
    assert!(bad.is_empty(), "eye too near the car:\n{}", bad.join("\n"));
}

#[test]
fn air_rolls_and_flips_do_not_swing_the_camera() {
    // The old camera put the eye 48 uu from the car at the bottom of a tumble.
    // Now it trails the travel direction from the same distance throughout.
    for ball in [false, true] {
        let mut sc = Scenario::new("jump, pitch back, air roll", 300, air_setup, air_input);
        sc.ball_cam = ball;
        let s = run(&sc);
        assert!(s.min_arm >= 800 && s.max_arm <= 900, "ball {ball}: arm {}..{}", s.min_arm, s.max_arm);
        assert_eq!(s.car_off, 0);
        let mut f = Scenario::new("front flip", 200, flip_setup, flip_input);
        f.ball_cam = ball;
        let s = run(&f);
        assert!(s.min_arm >= 800 && s.max_arm <= 900, "ball {ball}: flip arm {}..{}", s.min_arm, s.max_arm);
    }
}

#[test]
fn the_camera_levels_off_in_the_air_and_follows_the_roof() {
    // After the roof ride the car falls; the smoothed up must be back to
    // world up before the car lands.
    let sc = Scenario::new("side wall, up it and over the roof", 420, side_wall_setup, always_boost);
    let mut st = CameraState::EMPTY;
    let mut s = Sim::new();
    s.opponent_ai = false;
    (sc.setup)(&mut s);
    s.kickoff_ticks = 255;
    let rig = Rig::DEFAULT;
    let rq = Request { rig: &rig, screen: Screen::FULL, ball_cam: false, hold_car: true, split: false };
    let (mut max_tilt, mut airborne_ticks) = (0, 0);
    for t in 0..sc.ticks {
        s.tick(&boost());
        update(&mut st, &s, &s.car, &rq, t);
        max_tilt = max_tilt.max(st.tilt);
        if !s.car.grounded {
            airborne_ticks += 1;
            if airborne_ticks > 30 {
                assert_eq!(st.tilt, 0, "tick {t}: still tilted {} after {} airborne ticks", st.tilt, airborne_ticks);
            }
        } else {
            airborne_ticks = 0;
        }
    }
    assert!(max_tilt > 1900, "never reached the roof: {max_tilt}");
}

#[test]
fn open_floor_driving_is_steady_and_does_not_reverse() {
    for every in [1, 2] {
        let s = run(&Scenario::new("arc", 360, arc_setup, arc_input).every(every));
        assert_eq!(s.yaw_reversals, 0, "every {every}");
        assert!(s.min_arm >= 860 && s.max_arm <= 870, "every {every}: arm {}..{}", s.min_arm, s.max_arm);
        assert_eq!(s.car_y, (154, 154), "car stays put on screen");
    }
}

#[test]
fn the_approved_kickoff_view_is_unchanged() {
    // HANDOFF-2026-10-09: "kickoff camera steep and close approved". The old
    // camera's numbers for the straight boosted kickoff, car cam: the car
    // starts low at 187 and settles at 154 once the boom is long enough, the
    // eye starts 332 uu behind it and ends 800.
    let s = run(&kickoff(false, 1));
    assert_eq!(s.car_y, (154, 187));
    assert_eq!(s.ball_off, 9);
    assert_eq!(s.min_arm, 468);
}

#[test]
fn ball_cam_frames_the_car_in_the_lower_third_and_the_ball_above() {
    // Used to pin the car against the bottom edge (y 207..241 of 240).
    let s = run(&kickoff(true, 1));
    assert!(s.car_y.1 <= 210, "car as low as {}", s.car_y.1);
    assert!(s.car_y.0 >= 150, "car as high as {}", s.car_y.0);
    assert_eq!(s.ball_off, 0, "ball lost at kickoff");

    // A ball well above the car: both must stay in the frame.
    let s = run(&Scenario::new("high", 120, high_ball_setup, always_throttle).ball());
    assert!(s.car_y.1 <= 210, "car as low as {}", s.car_y.1);
    assert!(s.ball_off <= 8, "ball lost for {} frames", s.ball_off);
    // And a ball in flight over the car.
    let s = run(&Scenario::new("lob", 160, lob_setup, always_boost).ball());
    assert!(s.car_y.1 <= 210, "car as low as {}", s.car_y.1);
}

#[test]
fn the_planes_are_the_old_clamp() {
    // The arm's arena is the footprint the old `keep_inside` clamped to, just
    // written as half-spaces: sample the footprint and its surroundings.
    use crate::{CORNER, CORNER_JOINT_PLANES, HALF_X, HALF_Z};
    // The hard margin (50) less what the arm may borrow at the walls (40).
    const M: i32 = 10;
    fn keep_inside(mut x: i32, mut z: i32) -> (i32, i32) {
        x = x.clamp(-(HALF_X - M), HALF_X - M);
        z = z.clamp(-(HALF_Z - M), HALF_Z - M);
        let limit = CORNER - M * 3 / 2;
        let over = x.abs() + z.abs() - limit;
        if over > 0 {
            x -= x.signum() * over / 2;
            z -= z.signum() * over / 2;
        }
        for &(nx, nz, off) in &CORNER_JOINT_PLANES {
            let over = ((x.abs() * nx + z.abs() * nz) >> 12) - ((off >> 2) - M);
            if over > 0 {
                x -= x.signum() * (nx * over >> 12);
                z -= z.signum() * (nz * over >> 12);
            }
        }
        (x, z)
    }
    let mut checked = 0;
    for x in (-4400..=4400).step_by(37) {
        for z in (-5500..=5500).step_by(41) {
            let inside = inside_arena(x, z);
            let kept = keep_inside(x, z) == (x, z);
            // The two build their corner chamfers from slightly different margins, so
            // a point within a few uu of an edge may disagree.
            if inside != kept {
                let edge = [(6, 0), (-6, 0), (0, 6), (0, -6)]
                    .iter()
                    .any(|&(dx, dz)| inside_arena(x + dx, z + dz) != inside);
                assert!(edge, "({x}, {z}) inside {inside} kept {kept}");
            }
            checked += 1;
        }
    }
    assert!(checked > 10_000);
}

/// Bot against bot for a minute of play a match: jumps, flips, walls, the
/// roof, goals and the kickoffs after them, none of it scripted for the
/// camera. Both seats, both cameras, full and split views, rendered at 30 fps.
#[test]
fn the_camera_holds_through_whole_demo_matches() {
    use crate::{Sim, DEMO_MATCHES};
    let rig = Rig::DEFAULT;
    let mut worst_jump = 0;
    let mut frames = 0;
    for (n, m) in DEMO_MATCHES.iter().enumerate() {
        for &(ball, split, seat) in &[
            (false, false, 0),
            (false, false, 1),
            (true, false, 0),
            (true, false, 1),
            (false, true, 0),
            (true, true, 1),
        ] {
            let mut s = Sim::demo(m);
            let mut st = CameraState::EMPTY;
            let screen = if split { Screen::SPLIT } else { Screen::FULL };
            let mut last: Option<((i32, i32, i32), u32, (i32, i32, i32))> = None;
            for t in 0..3600u32 {
                s.tick(&Input::default());
                if t % 2 != 0 {
                    continue;
                }
                // What the renderer does: a goal puts every camera on the ball
                // without holding the car, and a kickoff after a goal starts the
                // camera over.
                let celebrating = s.goal_freeze > 0;
                if s.kickoff_ticks() == 0 {
                    st = CameraState::EMPTY;
                    last = None;
                }
                let rq = Request {
                    rig: &rig,
                    screen,
                    ball_cam: ball || celebrating,
                    hold_car: !celebrating,
                    split,
                };
                let subject = if seat == 0 { s.car } else { s.opponent };
                let pose = update(&mut st, &s, &subject, &rq, t);
                frames += 1;
                let car = (subject.p.x >> FP, subject.p.y >> FP, subject.p.z >> FP);
                let who = format!("match {n} ball {ball} split {split} seat {seat} tick {t}");
                if !celebrating {
                    let cs = project(&pose, screen, car);
                    assert!(on_screen(cs, screen), "{who}: car at {cs:?}");
                }
                assert!(inside_arena(pose.eye.0, pose.eye.2), "{who}: eye {:?} outside", pose.eye);
                let arm = (pose.eye.0 - car.0, pose.eye.1 - car.1, pose.eye.2 - car.2);
                assert!(
                    isqrt_i32(arm.0 * arm.0 + arm.1 * arm.1 + arm.2 * arm.2) >= 300,
                    "{who}: eye {arm:?} from the car"
                );
                // Motion of the eye. Not judged across a frame where the car itself
                // was put somewhere: a kickoff, or the sim lifting it out of the net.
                if let Some((prev, pt, prev_car)) = last {
                    let dt = (t - pt).max(1) as i32;
                    let moved = (car.0 - prev_car.0).abs().max((car.2 - prev_car.2).abs()) / dt;
                    if moved <= 100 {
                        let d = (pose.eye.0 - prev.0, pose.eye.1 - prev.1, pose.eye.2 - prev.2);
                        let j = isqrt_i32(d.0 * d.0 + d.1 * d.1 + d.2 * d.2) / dt;
                        worst_jump = worst_jump.max(j);
                        assert!(j <= MAX_EYE_JUMP_PER_TICK, "{who}: eye moved {j} uu a tick");
                    }
                }
                last = Some((pose.eye, t, car));
            }
        }
    }
    assert!(frames > 10_000);
    println!("worst eye move over {frames} frames: {worst_jump} uu a tick");
}

#[test]
#[ignore = "diagnostic: one demo match, one seat, a window of ticks (DEMO=match,ball,split,seat,from,to)"]
fn print_demo_window() {
    use crate::{Sim, DEMO_MATCHES};
    let spec: Vec<i32> = std::env::var("DEMO").unwrap_or_default().split(',').filter_map(|v| v.parse().ok()).collect();
    let (n, ball, split, seat, from, to) = (spec[0] as usize, spec[1] != 0, spec[2] != 0, spec[3], spec[4] as u32, spec[5] as u32);
    let rig = Rig::DEFAULT;
    let mut s = Sim::demo(&DEMO_MATCHES[n]);
    let mut st = CameraState::EMPTY;
    let screen = if split { Screen::SPLIT } else { Screen::FULL };
    for t in 0..to {
        s.tick(&Input::default());
        if t % 2 != 0 {
            continue;
        }
        let celebrating = s.goal_freeze > 0;
        if s.kickoff_ticks() == 0 {
            st = CameraState::EMPTY;
        }
        let rq = Request { rig: &rig, screen, ball_cam: ball || celebrating, hold_car: !celebrating, split };
        let subject = if seat == 0 { s.car } else { s.opponent };
        let pose = update(&mut st, &s, &subject, &rq, t);
        if t >= from {
            let car = (subject.p.x >> FP, subject.p.y >> FP, subject.p.z >> FP);
            println!(
                "{t:>5} car ({:>5},{:>5},{:>5}) up ({:>5},{:>5},{:>5}) g {} v ({:>4},{:>4}) | eye ({:>5},{:>5},{:>5}) L {:>4} R {:>5} az {:>4} tilt {:>4} side {:>2} | yaw {:>4} pit {:>4} | kt {}",
                car.0, car.1, car.2, subject.up.x, subject.up.y, subject.up.z, subject.grounded as i32,
                subject.v.x >> FP, subject.v.z >> FP, pose.eye.0, pose.eye.1, pose.eye.2, st.len, st.rise, st.az,
                st.tilt, st.side, pose.yaw, pose.pitch, s.kickoff_ticks()
            );
        }
    }
}
