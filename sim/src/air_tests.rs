// SPDX-License-Identifier: GPL-2.0-or-later
//! Host scenarios for the air game: air control, the dodge and air roll, each
//! driven through the real `Sim::tick` and checked against what RocketSim does
//! with the same inputs in an empty arena (see `PARITY.md`).
//!
//! `cargo test air_tests::trace -- --ignored --nocapture` prints one scenario
//! in the format of the reference trace (`NX_SCEN`, `NX_SPEED` in uu/s), for
//! diffing against it.

use crate::{uu, Car, Input, Sim, ANG_FP, CAR_JUMP_V, V3};

/// uu/s to sub-units a tick.
fn per_tick(uu_s: i32) -> i32 {
    uu_s * 64 / 60
}

/// A sim with the player car already in the air after its first jump, upright
/// at 1000 uu, heading +Z at `speed` uu/s, with the second jump available. The
/// opponent is parked out of the way. RL's trace starts the same way: a car in
/// an empty arena with the jump behind it, 292 uu/s up.
fn in_the_air(speed: i32) -> Sim {
    let mut sim = Sim::new();
    sim.opponent_ai = false;
    sim.opponent.p = V3::new(
        0,
        uu(crate::CAR_REST_Y),
        uu(crate::HALF_Z + crate::GOAL_DEPTH / 2),
    );
    sim.car = Car {
        p: V3::new(0, uu(1000), 0),
        v: V3::new(0, CAR_JUMP_V, per_tick(speed)),
        up: V3::new(0, 4096, 0),
        grounded: false,
        jumps_used: 1,
        dodge_window: crate::DODGE_WINDOW,
        ..Car::default()
    };
    sim
}

/// One scripted input in Rocket League's own terms (pitch -1 is stick
/// forward), applied from tick `from` on.
#[derive(Clone, Copy)]
struct Step {
    from: u32,
    pitch: f32,
    yaw: f32,
    roll: f32,
    jump: bool,
}

fn input_of(s: Step, press_now: bool) -> Input {
    Input {
        pitch: (-s.pitch * 128.0) as i32,
        steer: (s.yaw * 128.0) as i32,
        roll: (s.roll * 128.0) as i32,
        jump_pressed: s.jump && press_now,
        ..Input::default()
    }
}

/// Everything the trace prints about the car, in the frame it started in
/// (forward, right, up) as the reference prints it.
struct Pose {
    fwd: [f32; 3],
    up: [f32; 3],
    vel: [f32; 3],
    /// Pitch (about the right axis, nose up positive as RL prints it), yaw
    /// (left positive) and roll rates in rad/s.
    rates: [f32; 3],
}

fn pose(sim: &Sim) -> Pose {
    let (right, up, fwd) = sim.car.basis();
    let q = |a: i32| a as f32 / 4096.0;
    let w = sim.car.w;
    let rate = |axis: V3| {
        (w.x as i64 * axis.x as i64 + w.y as i64 * axis.y as i64 + w.z as i64 * axis.z as i64)
            as f32
            / 4096.0
            / (1i64 << ANG_FP) as f32
            * 60.0
    };
    Pose {
        fwd: [q(fwd.z), q(fwd.x), q(fwd.y)],
        up: [q(up.z), q(up.x), q(up.y)],
        vel: [
            sim.car.v.z as f32 * 15.0 / 16.0,
            sim.car.v.x as f32 * 15.0 / 16.0,
            sim.car.v.y as f32 * 15.0 / 16.0,
        ],
        // The reference prints w.right, w.up, w.forward with RL's axes, where
        // pitch up is negative about right; this sim's pitch is nose down
        // positive about right, the same sign, so these line up as printed.
        rates: [rate(right), rate(up), -rate(fwd)],
    }
}

/// Run a script for `ticks` and hand back the pose after each tick.
fn run(speed: i32, script: &[Step], ticks: u32) -> Vec<Pose> {
    let mut sim = in_the_air(speed);
    let mut out = Vec::new();
    for t in 0..ticks {
        let mut cur = None;
        for s in script {
            if t >= s.from {
                cur = Some(*s);
            }
        }
        let s = cur.unwrap();
        // A press lasts the tick it is scripted on.
        let press = script.iter().any(|x| x.jump && x.from == t);
        sim.tick(&input_of(s, press));
        out.push(pose(&sim));
    }
    out
}

fn scenario(name: &str) -> (Vec<Step>, u32) {
    let st = |from, pitch, yaw, roll, jump| Step {
        from,
        pitch,
        yaw,
        roll,
        jump,
    };
    match name {
        "front" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, -1., 0., 0., true),
                st(7, 0., 0., 0., false),
            ],
            90,
        ),
        "back" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, 1., 0., 0., true),
                st(7, 0., 0., 0., false),
            ],
            90,
        ),
        "side" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, 0., 1., 0., true),
                st(7, 0., 0., 0., false),
            ],
            90,
        ),
        "diag" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, -1., 1., 0., true),
                st(7, 0., 0., 0., false),
            ],
            90,
        ),
        "cancel" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, -1., 0., 0., true),
                st(7, 0., 0., 0., false),
                st(12, 1., 0., 0., false),
            ],
            90,
        ),
        "cancel_now" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, -1., 0., 0., true),
                st(7, 1., 0., 0., false),
            ],
            90,
        ),
        "stall" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, 0., -1., 1., true),
                st(7, 0., 0., 0., false),
            ],
            90,
        ),
        "dj" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, 0., 0., 0., true),
                st(7, 0., 0., 0., false),
            ],
            60,
        ),
        "pitch_release" => (
            vec![st(0, 1., 0., 0., false), st(30, 0., 0., 0., false)],
            90,
        ),
        "roll_release" => (
            vec![st(0, 0., 0., 1., false), st(30, 0., 0., 0., false)],
            90,
        ),
        "pitch_half" => (vec![st(0, 0.5, 0., 0., false)], 90),
        "yaw_half" => (vec![st(0, 0., 0.5, 0., false)], 90),
        "roll_half" => (vec![st(0, 0., 0., 0.5, false)], 90),
        "combo" => (vec![st(0, 1., 1., 1., false)], 90),
        "lock" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, 0., 1., 0., true),
                st(7, 0., 0., 0., false),
                st(45, 1., 0., 0., false),
            ],
            90,
        ),
        "half" => (
            vec![
                st(0, 0., 0., 0., false),
                st(6, 1., 0., 0., true),
                st(7, 0., 0., 0., false),
                st(18, -1., 0., 1., false),
            ],
            90,
        ),
        _ => panic!("unknown scenario {name}"),
    }
}

#[test]
#[ignore = "prints a trace for diffing against the reference"]
fn trace() {
    let name = std::env::var("NX_SCEN").unwrap_or_else(|_| "front".into());
    let speed: i32 = std::env::var("NX_SPEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let (script, ticks) = scenario(&name);
    for (k, p) in run(speed, &script, ticks).iter().enumerate() {
        println!(
            "{:.4}  {:.4} {:.4} {:.4}  {:.4} {:.4} {:.4}  {:.1} {:.1} {:.1}  {:.3} {:.3} {:.3}",
            (k + 1) as f32 / 60.0,
            p.fwd[0],
            p.fwd[1],
            p.fwd[2],
            p.up[0],
            p.up[1],
            p.up[2],
            p.vel[0],
            p.vel[1],
            p.vel[2],
            p.rates[0],
            p.rates[1],
            p.rates[2],
        );
    }
}

#[test]
#[ignore = "prints where each attract-demo match now scores"]
fn demo_goals() {
    for (n, m) in crate::DEMO_MATCHES.iter().enumerate() {
        let mut sim = Sim::demo(m);
        let mut tick = 0;
        let (mut jumps, mut flips) = (0, 0);
        let mut used = [0u8; 2];
        while sim.goal_freeze == 0 && tick < 3600 {
            tick += 1;
            sim.tick(&Input::default());
            for (k, car) in [&sim.car, &sim.opponent].into_iter().enumerate() {
                if car.jumps_used > used[k] {
                    jumps += 1;
                    if car.dodge_timer > 0 {
                        flips += 1;
                    }
                }
                used[k] = car.jumps_used;
            }
        }
        println!(
            "demo {n}: goal tick {tick} by {:?}, {jumps} jumps, {flips} flips (was {} {:?})",
            sim.last_scorer, m.first_goal, m.scorer
        );
    }
}

// ---- fixtures from RocketSim -------------------------------------------------
//
// Every number below was measured by running RocketSim (c2baacb, the library
// RL's physics constants are taken from) in an empty arena with the same car
// and the same inputs, sampled at 60 Hz. The oracle that produced them is not
// in this repository; `PARITY.md` says how it was run.

/// The length of the car's spin, in rad/s.
fn spin_rad_s(sim: &Sim) -> f32 {
    crate::spin_len(sim.car.w) as f32 * 60.0 / (1i64 << ANG_FP) as f32
}

/// Spin after 15, 30 and 60 ticks (0.25, 0.5 and 1 s) of one stick held.
fn held(script: Step) -> [f32; 3] {
    let mut sim = in_the_air(0);
    let mut out = [0.0; 3];
    for t in 1..=60 {
        sim.tick(&input_of(script, false));
        match t {
            15 => out[0] = spin_rad_s(&sim),
            30 => out[1] = spin_rad_s(&sim),
            60 => out[2] = spin_rad_s(&sim),
            _ => {}
        }
    }
    out
}

fn near(got: f32, want: f32, tolerance: f32, what: &str) {
    assert!(
        (got - want).abs() <= tolerance,
        "{what}: got {got:.3}, RL {want:.3}"
    );
}

fn stick(pitch: f32, yaw: f32, roll: f32) -> Step {
    Step {
        from: 0,
        pitch,
        yaw,
        roll,
        jump: false,
    }
}

#[test]
fn air_control_matches_rocket_league() {
    // Full stick: RL reaches 3.12 / 5.50 / 5.50 rad/s in pitch, 2.28 / 4.55 /
    // 5.50 in yaw, and is pinned at the 5.5 cap in roll before a quarter
    // second is out.
    for (name, got, want) in [
        ("pitch", held(stick(1.0, 0.0, 0.0)), [3.12, 5.50, 5.50]),
        ("yaw", held(stick(0.0, 1.0, 0.0)), [2.28, 4.55, 5.50]),
        ("roll", held(stick(0.0, 0.0, 1.0)), [5.50, 5.50, 5.50]),
    ] {
        for k in 0..3 {
            near(
                got[k],
                want[k],
                0.06 + want[k] * 0.02,
                &format!("{name} full stick, sample {k}"),
            );
        }
    }
}

#[test]
fn a_half_stick_is_half_the_torque_and_a_looser_damper() {
    // The analog stick is respected: half a stick is half the torque, and
    // pitch and yaw damping relax as the stick is pushed, so the curve is not
    // simply half of the full one. RL: pitch 1.32 / 2.23 / 3.31, yaw 1.02 /
    // 1.82 / 2.94, roll 2.82 / 3.65 / 3.97.
    for (name, got, want) in [
        ("pitch", held(stick(0.5, 0.0, 0.0)), [1.315, 2.231, 3.314]),
        ("yaw", held(stick(0.0, 0.5, 0.0)), [1.016, 1.815, 2.936]),
        ("roll", held(stick(0.0, 0.0, 0.5)), [2.823, 3.653, 3.970]),
    ] {
        for k in 0..3 {
            near(
                got[k],
                want[k],
                0.06 + want[k] * 0.02,
                &format!("{name} half stick, sample {k}"),
            );
        }
    }
}

#[test]
fn a_released_stick_coasts_down_on_the_damper() {
    // Held for half a second, then let go: RL's spin is 5.50 at release and
    // 2.66 / 1.28 / 0.30 rad/s a quarter, half and one second after.
    // Roll: 5.50, then 1.62 / 0.48 / 0.04.
    for (axis, step, want) in [
        ("pitch", stick(1.0, 0.0, 0.0), [2.656, 1.283, 0.299]),
        ("roll", stick(0.0, 0.0, 1.0), [1.619, 0.477, 0.041]),
    ] {
        let mut sim = in_the_air(0);
        for _ in 0..30 {
            sim.tick(&input_of(step, false));
        }
        near(spin_rad_s(&sim), 5.5, 0.05, &format!("{axis} at release"));
        let mut got = [0.0; 3];
        for t in 1..=60 {
            sim.tick(&Input::default());
            match t {
                15 => got[0] = spin_rad_s(&sim),
                30 => got[1] = spin_rad_s(&sim),
                60 => got[2] = spin_rad_s(&sim),
                _ => {}
            }
        }
        for k in 0..3 {
            near(
                got[k],
                want[k],
                0.12,
                &format!("{axis} coasting, sample {k}"),
            );
        }
    }
}

#[test]
fn all_three_sticks_at_once_stay_under_the_cap() {
    // RL scales the whole spin to 5.5 rad/s, not each axis. All three held,
    // it settles at 2.87 / 2.10 / 4.20 per axis (5.5 overall).
    let mut sim = in_the_air(0);
    for t in 0..120 {
        sim.tick(&input_of(stick(1.0, 1.0, 1.0), false));
        assert!(
            spin_rad_s(&sim) < 5.52,
            "tick {t}: spin {} rad/s over the cap",
            spin_rad_s(&sim)
        );
    }
    near(
        spin_rad_s(&sim),
        5.5,
        0.06,
        "the three-axis spin settles at the cap",
    );
    let p = pose(&sim);
    near(
        p.rates[0].abs(),
        2.87,
        0.25,
        "pitch share of the capped spin",
    );
    near(p.rates[1].abs(), 2.10, 0.25, "yaw share");
    near(p.rates[2].abs(), 4.20, 0.25, "roll share");
}

/// RL's fwd and up vectors, in the start frame (forward, right, up), in
/// thousandths, at 18, 30, 45, 60 and 90 ticks (0.3 s, 0.5 s, 0.75 s, 1 s and
/// 1.5 s) into each scenario; flips are pressed at 0.1 s.
const REFERENCE: [(&str, u32, [i16; 6]); 40] = [
    ("front", 18, [243, 0, -969, 969, 0, 243]),
    ("front", 30, [-941, 0, -337, 337, 0, -941]),
    ("front", 45, [-72, 0, 997, -997, 0, -72]),
    ("front", 60, [802, 0, 596, -596, 0, 802]),
    ("front", 90, [997, 0, -68, 68, 0, 997]),
    ("back", 18, [243, 0, 969, -969, 0, 243]),
    ("back", 30, [-941, 0, 337, -337, 0, -941]),
    ("back", 45, [-72, 0, -997, 997, 0, -72]),
    ("back", 60, [802, 0, -596, 596, 0, 802]),
    ("back", 90, [997, 0, 68, -68, 0, 997]),
    ("side", 18, [1000, 4, 0, -4, 983, 183]),
    ("side", 30, [1000, 4, 0, -1, 220, -975]),
    ("side", 45, [1000, 4, 0, 4, -992, 123]),
    ("side", 60, [1000, 4, 0, 3, -580, 814]),
    ("side", 90, [1000, 4, 0, 1, -305, 952]),
    ("diag", 18, [663, -384, -641, 635, 742, 212]),
    ("diag", 30, [167, -968, -184, 175, 213, -961]),
    ("diag", 45, [591, -479, 648, -649, -759, 29]),
    ("diag", 60, [937, -44, 344, -318, -507, 800]),
    ("diag", 90, [988, 131, -74, 107, -270, 956]),
    ("cancel", 18, [489, 0, -872, 872, 0, 489]),
    ("cancel", 30, [-104, 0, -994, 994, 0, -104]),
    ("cancel", 45, [-485, 0, -874, 874, 0, -485]),
    ("cancel", 60, [-645, 0, -763, 763, 0, -645]),
    ("cancel", 90, [303, 0, -952, 952, 0, 303]),
    ("cancel_now", 18, [957, 0, -289, 289, 0, 957]),
    ("cancel_now", 30, [893, 0, -450, 450, 0, 893]),
    ("cancel_now", 45, [836, 0, -548, 548, 0, 836]),
    ("cancel_now", 60, [805, 0, -593, 593, 0, 805]),
    ("cancel_now", 90, [857, 0, 513, -513, 0, 857]),
    ("stall", 18, [999, -25, 1, 0, 82, 996]),
    ("stall", 30, [999, -42, 2, 2, 113, 993]),
    ("stall", 45, [998, -56, 4, 2, 127, 991]),
    ("stall", 60, [997, -65, 5, 2, 131, 991]),
    ("stall", 90, [997, -74, 6, 3, 132, 991]),
    ("half", 30, [-517, 146, 843, -678, 531, -507]),
    ("half", 45, [-602, 465, 648, 504, 851, -142]),
    ("half", 60, [-516, 528, 673, 737, -125, 663]),
    ("half", 90, [12, 881, 472, -971, 122, -202]),
    ("half", 18, [243, 0, 969, -969, 0, 243]),
];

fn degrees_between(a: [f32; 3], b: [i16; 3]) -> f32 {
    let (b0, b1, b2) = (
        b[0] as f32 / 1000.0,
        b[1] as f32 / 1000.0,
        b[2] as f32 / 1000.0,
    );
    let dot = (a[0] * b0 + a[1] * b1 + a[2] * b2).clamp(-1.0, 1.0);
    dot.acos().to_degrees()
}

#[test]
fn flips_turn_the_car_the_way_rocket_league_does() {
    // Front, back, side and diagonal flips, a cancelled flip, a stall and a
    // half-flip, compared by the direction of the car's nose and roof against
    // RL's at five moments. The sim runs at half RL's rate and starts a
    // flip's torque on a 60 Hz boundary, so the bar is eight degrees: the
    // worst measured is 7.3, on the roll of a side flip, which carries
    // furthest.
    let mut worst = 0.0f32;
    for (name, tick, want) in REFERENCE {
        let (script, ticks) = scenario(name);
        let poses = run(0, &script, ticks);
        let p = &poses[tick as usize - 1];
        let nose = degrees_between(p.fwd, [want[0], want[1], want[2]]);
        let roof = degrees_between(p.up, [want[3], want[4], want[5]]);
        worst = worst.max(nose).max(roof);
        assert!(
            nose < 8.0 && roof < 8.0,
            "{name} at tick {tick}: nose {nose:.1} deg, roof {roof:.1} deg off RL"
        );
    }
    std::println!("worst flip attitude error {worst:.1} deg");
}

/// RL's velocity (forward, right) in uu/s after the flip, from a car doing
/// 0, 700 and 1400 uu/s along its nose: the kick is 500 uu/s scaled by
/// 1 + (scale - 1) * speed / 2300, scale 1.0 forward, 1.9 sideways and 2.5
/// backward (and a further 16/15 on a backward flip).
const KICKS: [(&str, i32, f32, f32); 12] = [
    ("front", 0, 500.0, 0.0),
    ("front", 700, 1200.0, 0.0),
    ("front", 1400, 1900.0, 0.0),
    ("back", 0, -533.3, 0.0),
    ("back", 700, -76.8, 0.0),
    ("back", 1400, 379.7, 0.0),
    ("side", 0, 0.0, 500.0),
    ("side", 700, 700.0, 637.0),
    ("side", 1400, 1400.0, 773.9),
    ("diag", 0, 353.6, 353.6),
    ("diag", 700, 1053.6, 450.4),
    ("diag", 1400, 1753.6, 547.2),
];

#[test]
fn a_flip_kicks_by_direction_and_speed_like_rocket_league() {
    for (name, speed, forward, right) in KICKS {
        let (script, ticks) = scenario(name);
        let p = &run(speed, &script, ticks)[17];
        near(
            p.vel[0],
            forward,
            6.0,
            &format!("{name} at {speed} uu/s, forward"),
        );
        near(
            p.vel[1],
            right,
            6.0,
            &format!("{name} at {speed} uu/s, sideways"),
        );
    }
}

#[test]
fn a_flip_adds_its_kick_on_top_of_the_throttle_top_speed() {
    // The throttle's 1410 uu/s is an engine limit and the flip is not the
    // engine: at 1400 a front flip must reach 1900, not be cut back to where
    // it started. The 2300 ceiling still holds.
    let (script, ticks) = scenario("front");
    let fast = run(1400, &script, ticks);
    assert!(
        fast[17].vel[0] > 1850.0,
        "the kick was clipped: {}",
        fast[17].vel[0]
    );
    let top = run(2200, &script, ticks);
    for p in &top {
        let speed = (p.vel[0] * p.vel[0] + p.vel[1] * p.vel[1] + p.vel[2] * p.vel[2]).sqrt();
        // The ceiling is applied before gravity, so a falling car can read
        // a tick or two of gravity (11 uu/s each) over it.
        assert!(
            speed < 2340.0,
            "a flip took the car past the ceiling: {speed}"
        );
    }
}

#[test]
fn a_stall_turns_nothing_and_kicks_nothing() {
    // Roll held right with the stick yawed left cancels to no direction: RL
    // lets the roll through and adds no spin and no kick.
    let (script, ticks) = scenario("stall");
    let poses = run(700, &script, ticks);
    for p in &poses[10..] {
        assert!(
            (p.vel[0] - 700.0).abs() < 8.0,
            "a stall kicked the car: {}",
            p.vel[0]
        );
        assert!(
            p.vel[1].abs() < 8.0,
            "a stall kicked the car sideways: {}",
            p.vel[1]
        );
        assert!(p.fwd[0] > 0.97, "a stall turned the car over: {:?}", p.fwd);
    }
}

#[test]
fn pitch_stays_locked_out_after_a_flip_for_the_time_rocket_league_says() {
    // A side flip, then the stick pulled back from 0.75 s: the pitch does
    // nothing until 0.95 s after the flip started (tick 63 here), then takes
    // hold. RL's nose has not left the horizon at tick 63 and is 20 degrees up
    // by tick 72.
    let (script, ticks) = scenario("lock");
    let poses = run(0, &script, ticks);
    assert!(
        poses[60].fwd[2].abs() < 0.03,
        "pitch bit during the lock: {:?}",
        poses[60].fwd
    );
    assert!(
        poses[71].fwd[2].abs() > 0.06,
        "pitch never came back: {:?}",
        poses[71].fwd
    );
    assert!(
        poses[89].fwd[2].abs() > 0.6,
        "and then it should be rotating hard: {:?}",
        poses[89].fwd
    );
}

#[test]
fn a_cancelled_flip_stops_turning() {
    // Pulling back against a front flip the moment it starts scales its
    // torque away and puts the damper back on the pitch, so the car turns far
    // less than an uncancelled flip does: RL's nose is 33 degrees round at
    // 0.75 s against 94 for the full flip, and is back near level by 1.5 s
    // where the flip has gone the whole way over.
    let (full, ticks) = scenario("front");
    let (cancelled, _) = scenario("cancel_now");
    let a = run(0, &full, ticks);
    let b = run(0, &cancelled, ticks);
    let turned = |p: &Pose| p.fwd[0].clamp(-1.0, 1.0).acos().to_degrees();
    assert!(
        turned(&a[44]) > 80.0,
        "the full flip should be well over by now"
    );
    assert!(
        turned(&b[44]) < turned(&a[44]) - 40.0,
        "cancel did not stop the flip"
    );
}

#[test]
fn directional_air_roll_rolls_the_right_side_down() {
    // Roll right: the roof tips toward the car's right, the way RL's does,
    // whether it comes from the roll button or from the free-roll stick.
    for input in [
        Input {
            roll: 128,
            ..Input::default()
        },
        Input {
            steer: 128,
            air_roll: true,
            ..Input::default()
        },
    ] {
        let mut sim = in_the_air(0);
        for _ in 0..20 {
            sim.tick(&input);
        }
        let (right, up, _) = sim.car.basis();
        assert!(
            crate::dot_q12(up, V3::new(4096, 0, 0)) > 1500,
            "roof did not lean right: {up:?}"
        );
        assert!(right.y < -1500, "right side did not go down: {right:?}");
    }
    // And left is left.
    let mut sim = in_the_air(0);
    for _ in 0..20 {
        sim.tick(&Input {
            roll: -128,
            ..Input::default()
        });
    }
    assert!(sim.car.basis().1.x < -1500);
}

#[test]
fn free_air_roll_keeps_the_pitch_axis_live() {
    // L1 held: the stick's x is roll, its y is still pitch, so a car can be
    // pitched and rolled at once.
    let mut sim = in_the_air(0);
    for _ in 0..20 {
        sim.tick(&Input {
            steer: 128,
            pitch: -128,
            air_roll: true,
            ..Input::default()
        });
    }
    let w = sim.car.w;
    assert!(w.x != 0 || w.y != 0 || w.z != 0);
    let (_, up, fwd) = sim.car.basis();
    assert!(fwd.y > 500, "the nose should have come up: {fwd:?}");
    assert!(up.x > 500, "and the roof leant right: {up:?}");
}

#[test]
fn the_attitude_follows_the_spin_it_integrates() {
    // Yaw held for a second and a half: the heading change must equal the
    // area under the spin curve, to a couple of percent. This is the check on
    // the integration itself, apart from the torque model.
    let mut sim = in_the_air(0);
    let mut area = 0.0f32;
    let start = sim.car.yaw;
    for _ in 0..90 {
        sim.tick(&Input {
            steer: 128,
            ..Input::default()
        });
        area += spin_rad_s(&sim) / 60.0;
    }
    let turned = sim.car.yaw.wrapping_sub(start) as f32 / 4096.0 * core::f32::consts::TAU;
    let wrapped = if turned > core::f32::consts::PI {
        turned - core::f32::consts::TAU
    } else {
        turned
    };
    let total = area % core::f32::consts::TAU;
    let want = if total > core::f32::consts::PI {
        total - core::f32::consts::TAU
    } else {
        total
    };
    assert!(
        (wrapped - want).abs() < 0.06,
        "turned {wrapped:.3} rad, integral {want:.3} rad"
    );
}

#[test]
fn a_car_lands_pointing_where_its_nose_pointed() {
    // Pitched hard either way and left to fall, heading east: it lands facing
    // east, not wherever the stored yaw happens to mean once `up` has moved.
    for (burst, pitch) in [(8, -128), (12, -128), (8, 128), (12, 128)] {
        let mut sim = in_the_air(0);
        sim.car.p = V3::new(0, uu(1800), 0);
        sim.car.v = V3::ZERO;
        sim.car.yaw = 1024; // east
        for _ in 0..burst {
            sim.tick(&Input {
                pitch,
                ..Input::default()
            });
        }
        // The spin carries on after the stick is let go: the car is well
        // tipped by the time the damper has stopped it.
        for _ in 0..40 {
            sim.tick(&Input::default());
        }
        assert!(
            sim.car.up.y < 3600,
            "a burst of {burst} should have tipped the car: {:?}",
            sim.car.up
        );
        for _ in 0..300 {
            sim.tick(&Input::default());
        }
        assert!(sim.car.grounded, "pitch {pitch} for {burst}: never landed");
        let (_, _, f) = sim.car.basis();
        assert!(
            f.x > 3700 && f.z.abs() < 1000,
            "pitch {pitch} for {burst}: landed facing {f:?}, not east"
        );
    }
}

#[test]
fn an_aerial_replays_to_the_same_state() {
    // A scripted aerial: jump, pitch up, boost, a front flip, a roll. Run
    // twice from scratch it must land in bit-identical state (all integer
    // arithmetic, no clock, no hidden state), and it pins the numbers so a
    // physics change to the air game cannot go unnoticed.
    fn go() -> (Car, Car) {
        let mut sim = Sim::new();
        sim.opponent_ai = false;
        let mut mid = Car::default();
        for t in 0..320u32 {
            let input = match t {
                0..=59 => Input {
                    throttle: 128,
                    boost: true,
                    ..Input::default()
                },
                60..=89 => Input {
                    throttle: 128,
                    boost: true,
                    jump_pressed: t == 60,
                    jump_held: true,
                    ..Input::default()
                },
                90..=119 => Input {
                    throttle: 128,
                    boost: true,
                    pitch: -128,
                    ..Input::default()
                },
                120..=125 => Input {
                    pitch: 128,
                    jump_pressed: t == 120,
                    ..Input::default()
                },
                126..=170 => Input {
                    roll: 128,
                    steer: -40,
                    ..Input::default()
                },
                _ => Input::default(),
            };
            sim.tick(&input);
            if t == 150 {
                mid = sim.car;
            }
        }
        (mid, sim.car)
    }
    let (a, b) = (go(), go());
    assert_eq!(a, b);
    let (mid, end) = a;
    assert!(!mid.grounded, "tick 150 should be mid-air");
    assert_eq!(
        (mid.p, mid.v, mid.up, mid.yaw, mid.w, mid.dodge_timer),
        (
            V3::new(0, 14140, -41842),
            V3::new(0, -19, 1916),
            V3::new(0, -930, 3988),
            3072,
            V3::new(96112, 0, 0),
            8,
        ),
        "the air game's physics changed: re-pin this only with a reason"
    );
    assert_eq!(
        (end.p, end.yaw, end.grounded),
        (V3::new(21690, 1792, 87725), 2770, true)
    );
}
